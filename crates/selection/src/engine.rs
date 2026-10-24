//! The engine: routes events and commands to capture, keep-alive, the clipboard-manager handoff
//! and serving (docs/selection.md).

use std::sync::Arc;

use jackstay_clip::{Content, Format, TargetName};

use crate::atoms::{Known, MAX_ATOM_LIST_BYTES};
use crate::capture::{ANSWER_MS, Capture, Ctx, Limits, Progress, answers};
use crate::pool::Pool;
use crate::serve::{Request, Served, Transfers};
use crate::{Atom, Event, Millis, OwnerChange, Port, PropertyState, Read, Timestamp, Window};

/// How long an exiting application's `SAVE_TARGETS` may take before it is answered with what is
/// held. GTK 3 waits 10 s and Qt 5 s.
pub(crate) const SAVE_BUDGET_MS: u64 = 4_000;
/// How often an idle owner is asked for `TARGETS` to see that it still answers.
pub(crate) const PROBE_EVERY_MS: u64 = 30_000;
/// Unanswered probes in a row before the owner is reported hung.
const PROBE_MISSES: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub limits: Limits,
}

/// What the engine tells the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// Another client claimed the clipboard, and capturing what it offers has begun.
    CaptureStarted { capture: u64 },
    /// Content captured so far. Later reports with the same `capture` replace earlier ones;
    /// `complete` once nothing more will be fetched.
    Captured { capture: u64, content: Arc<Content>, source: Option<String>, complete: bool },
    /// A capture that had reported content was cut short (a newer copy, the owner's death, a
    /// clear): its last report stands, and no more will come.
    CaptureEnded { capture: u64 },
    /// A capture ended with nothing kept.
    CaptureFailed { capture: u64 },
    /// The owner exited and the engine now serves its last copy.
    KeptAlive,
    /// A `serve`, `clear` or `restore` claim took effect.
    Claimed,
    /// A `serve`, `clear` or `restore` claim did not: another client owns the clipboard.
    ClaimFailed,
    /// Another client took the clipboard from us.
    Lost,
    /// Another clipboard manager owns `CLIPBOARD_MANAGER`.
    ManagerConflict,
    /// Another client took `CLIPBOARD_MANAGER` from us.
    ManagerLost,
    /// The clipboard's owner stopped answering.
    OwnerHung,
    /// An exiting application's `SAVE_TARGETS` was answered; `taken` if we took the clipboard.
    SaveAnswered { taken: bool },
}

#[derive(Debug)]
enum Ownership {
    /// Another client owns it, since `selection_time`.
    Theirs {
        selection_time: Timestamp,
    },
    Ours(Served),
    Unowned,
}

#[derive(Debug)]
struct Candidate {
    selection_time: Timestamp,
    content: Arc<Content>,
}

#[derive(Debug)]
struct Save {
    requestor: Window,
    property: Atom,
    time: Timestamp,
    deadline: Millis,
}

#[derive(Debug, Default)]
struct Probe {
    next_at: Option<Millis>,
    outstanding: Option<(Atom, Millis)>,
    misses: u8,
}

/// The clipboard's selection engine. CLIPBOARD only; PRIMARY is neither captured nor served in M1,
/// except that a clear claims it too, offering nothing.
#[derive(Debug)]
pub struct Engine {
    known: Known,
    pool: Pool,
    our_window: Window,
    config: Config,
    ownership: Ownership,
    /// PRIMARY, while a clear holds it.
    primary_cleared: Option<Served>,
    capture: Option<Capture>,
    capture_seq: u64,
    /// Whether the current capture has reported content but not yet completion.
    capture_open: bool,
    candidate: Option<Candidate>,
    transfers: Transfers,
    manager_owned: bool,
    save: Option<Save>,
    probe: Probe,
}

impl Engine {
    /// Starts the engine on `our_window`: claims `CLIPBOARD_MANAGER` unless another manager holds
    /// it, and captures the clipboard's current owner, if any. That owner's claim time is not
    /// known, so if it exits it is not kept alive (docs/selection.md, "Keep-alive").
    pub fn new(
        port: &mut impl Port,
        our_window: Window,
        config: Config,
        now: Millis,
    ) -> (Self, Vec<Report>) {
        let known = Known::intern(port);
        let transfers = Transfers::new(port.max_property_bytes());
        let mut engine = Engine {
            known,
            pool: Pool::default(),
            our_window,
            config,
            ownership: Ownership::Unowned,
            primary_cleared: None,
            capture: None,
            capture_seq: 0,
            capture_open: false,
            candidate: None,
            transfers,
            manager_owned: false,
            save: None,
            probe: Probe::default(),
        };
        let mut reports = Vec::new();
        let manager = engine.known.clipboard_manager;
        match port.selection_owner(manager) {
            Some(other) if other != our_window => reports.push(Report::ManagerConflict),
            _ => {
                let time = port.server_time();
                port.set_selection_owner(manager, Some(our_window), time);
                if port.selection_owner(manager) == Some(our_window) {
                    engine.manager_owned = true;
                    port.announce_manager(manager, time);
                } else {
                    reports.push(Report::ManagerConflict);
                }
            }
        }
        let clipboard = engine.known.clipboard;
        if let Some(owner) = port.selection_owner(clipboard).filter(|&o| o != our_window) {
            engine.start_capture(port, owner, Timestamp::CURRENT_TIME, now, &mut reports);
        }
        (engine, reports)
    }

    /// When the engine next needs [`Engine::tick`], if it is waiting on anything.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Millis> {
        let probe = match self.ownership {
            Ownership::Theirs { .. } => {
                self.probe.outstanding.map(|(_, d)| d).or(self.probe.next_at)
            }
            Ownership::Ours(_) | Ownership::Unowned => None,
        };
        [
            self.capture.as_ref().and_then(Capture::deadline),
            self.transfers.deadline(),
            self.save.as_ref().map(|s| s.deadline),
            probe,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Whether the engine believes it owns CLIPBOARD.
    #[must_use]
    pub fn owns_clipboard(&self) -> bool {
        matches!(self.ownership, Ownership::Ours(_))
    }

    /// How many of the engine's transfer properties are in use: at most one for a capture and one
    /// for the liveness probe.
    #[must_use]
    pub fn properties_in_use(&self) -> usize {
        self.pool.in_use()
    }

    /// Handles one event from the X server.
    pub fn handle(&mut self, port: &mut impl Port, event: Event, now: Millis) -> Vec<Report> {
        let mut reports = Vec::new();
        match event {
            Event::OwnerChanged { selection, owner, change, selection_timestamp, .. }
                if selection == self.known.clipboard =>
            {
                self.owner_changed(port, owner, change, selection_timestamp, now, &mut reports);
            }
            Event::SelectionNotify { selection, target, property, time }
                if selection == self.known.clipboard =>
            {
                if self.probe_answered(port, target, property, time, now) {
                    return reports;
                }
                let progress =
                    self.with_capture(port, now, |c, cx| c.on_notify(cx, target, property, time));
                self.progressed(progress, &mut reports);
                self.finish_save_if_ready(port, now, &mut reports);
            }
            Event::SelectionRequest { requestor, selection, target, property, time } => {
                let req = Request { requestor, selection, target, property, time };
                self.request(port, &req, now, &mut reports);
            }
            Event::SelectionClear { selection, time } => {
                if selection == self.known.clipboard {
                    if self.owns_clipboard() && !self.stale(time) {
                        self.ownership = Ownership::Unowned;
                        self.cancel_probe(now);
                        reports.push(Report::Lost);
                    }
                } else if selection == self.known.clipboard_manager && self.manager_owned {
                    self.manager_owned = false;
                    reports.push(Report::ManagerLost);
                } else if selection == self.known.primary
                    && self.primary_cleared.as_ref().is_some_and(|s| !time.is_earlier_than(s.since))
                {
                    self.primary_cleared = None;
                }
            }
            Event::PropertyNotify { window, atom, state, .. } => {
                if window == self.our_window && state == PropertyState::NewValue {
                    // A late write to one of our idle properties (an owner answering a
                    // conversion we gave up on) is deleted at once, rather than left holding
                    // what may be megabytes in the server; for an INCR owner the delete also
                    // asks for its next chunk, so a stale transfer drains instead of hanging.
                    if self.pool.is_idle(atom) {
                        port.delete_property(self.our_window, atom);
                        return reports;
                    }
                    let progress = self.with_capture(port, now, |c, cx| c.on_new_value(cx, atom));
                    self.progressed(progress, &mut reports);
                    self.finish_save_if_ready(port, now, &mut reports);
                } else if state == PropertyState::Deleted {
                    // Transfers are keyed by requestor window, never ours, so the deletes our own
                    // reads cause find no transfer.
                    self.transfers.on_deleted(port, window, atom, now);
                }
            }
            Event::DestroyNotify { window } => self.transfers.on_destroyed(window),
            // Other selections: PRIMARY is neither captured nor served in M1.
            Event::OwnerChanged { .. } | Event::SelectionNotify { .. } => {}
        }
        reports
    }

    /// Handles the passage of time: missed deadlines, idle transfers, the liveness probe.
    pub fn tick(&mut self, port: &mut impl Port, now: Millis) -> Vec<Report> {
        let mut reports = Vec::new();
        let progress = self.with_capture(port, now, Capture::on_tick);
        self.progressed(progress, &mut reports);
        self.transfers.on_tick(port, now);
        if self.save.as_ref().is_some_and(|s| s.deadline <= now) {
            self.end_capture(now, &mut reports);
            self.finish_save(port, now, &mut reports);
        }
        self.finish_save_if_ready(port, now, &mut reports);
        self.probe_tick(port, now, &mut reports);
        reports
    }

    /// Serves `content`, which the user re-selected: claims the clipboard with a fresh timestamp.
    pub fn serve(
        &mut self,
        port: &mut impl Port,
        content: Arc<Content>,
        now: Millis,
    ) -> Vec<Report> {
        let time = port.server_time();
        self.claim(port, Some(content), time, now)
    }

    /// Clears the clipboard: abandons any capture, forgets the keep-alive candidate, claims
    /// CLIPBOARD and PRIMARY offering nothing, and empties the cut buffers. The report is
    /// CLIPBOARD's; PRIMARY is claimed whether or not that succeeded.
    pub fn clear(&mut self, port: &mut impl Port, now: Millis) -> Vec<Report> {
        let time = port.server_time();
        let reports = self.claim(port, None, time, now);
        let primary = self.known.primary;
        port.set_selection_owner(primary, Some(self.our_window), time);
        self.primary_cleared = (port.selection_owner(primary) == Some(self.our_window))
            .then(|| Served::new(port, None, time));
        port.clear_cut_buffers();
        reports
    }

    /// Serves `content`, saved from an earlier run, but only if no client owns the clipboard: at
    /// start, a live application's copy is newer than anything saved. Reports `Claimed` if it is
    /// served, else `ClaimFailed`.
    pub fn restore(
        &mut self,
        port: &mut impl Port,
        content: Arc<Content>,
        now: Millis,
    ) -> Vec<Report> {
        if !matches!(self.ownership, Ownership::Unowned) {
            return vec![Report::ClaimFailed];
        }
        // Under a grab no client can claim between the check and our claim; one that claims after
        // the grab has a later time than ours, and the server keeps its claim.
        let clipboard = self.known.clipboard;
        port.grab_server(true);
        let time = port.server_time();
        let unowned = port.selection_owner(clipboard).is_none();
        if unowned {
            port.set_selection_owner(clipboard, Some(self.our_window), time);
        }
        port.grab_server(false);
        if !unowned || port.selection_owner(clipboard) != Some(self.our_window) {
            return vec![Report::ClaimFailed];
        }
        // Every transition settles what is pending, though an unowned clipboard has little.
        let mut reports = Vec::new();
        self.end_capture(now, &mut reports);
        self.abort_save(port, &mut reports);
        self.cancel_probe(now);
        self.candidate = None;
        self.ownership = Ownership::Ours(Served::new(port, Some(content), time));
        reports.push(Report::Claimed);
        reports
    }

    /// An event about CLIPBOARD from before our current claim: the server has already put it
    /// behind us, so it must not undo our ownership.
    fn stale(&self, time: Timestamp) -> bool {
        match &self.ownership {
            Ownership::Ours(served) => time.is_earlier_than(served.since),
            Ownership::Theirs { .. } | Ownership::Unowned => false,
        }
    }

    fn claim(
        &mut self,
        port: &mut impl Port,
        content: Option<Arc<Content>>,
        time: Timestamp,
        now: Millis,
    ) -> Vec<Report> {
        let mut reports = Vec::new();
        self.end_capture(now, &mut reports);
        self.abort_save(port, &mut reports);
        self.cancel_probe(now);
        self.candidate = None;
        let clipboard = self.known.clipboard;
        port.set_selection_owner(clipboard, Some(self.our_window), time);
        if port.selection_owner(clipboard) == Some(self.our_window) {
            self.ownership = Ownership::Ours(Served::new(port, content, time));
            reports.push(Report::Claimed);
        } else {
            reports.push(Report::ClaimFailed);
        }
        reports
    }

    fn owner_changed(
        &mut self,
        port: &mut impl Port,
        owner: Option<Window>,
        change: OwnerChange,
        selection_time: Timestamp,
        now: Millis,
        reports: &mut Vec<Report>,
    ) {
        match change {
            OwnerChange::Set if owner == Some(self.our_window) => {}
            OwnerChange::Set if self.stale(selection_time) => {}
            OwnerChange::Set => {
                self.end_capture(now, reports);
                self.abort_save(port, reports);
                self.cancel_probe(now);
                self.candidate = None;
                match owner {
                    Some(owner) => {
                        self.start_capture(port, owner, selection_time, now, reports);
                    }
                    None => self.ownership = Ownership::Unowned,
                }
            }
            OwnerChange::WindowDestroyed | OwnerChange::ClientClosed => {
                // Only the death of the owner we believe holds the clipboard counts. Death events
                // carry no owner; the selection time, unchanged by the death, identifies it.
                let Ownership::Theirs { selection_time: ours } = self.ownership else { return };
                if ours != selection_time {
                    return;
                }
                self.end_capture(now, reports);
                self.cancel_probe(now);
                self.ownership = Ownership::Unowned;
                let kept = self.keep_alive(port, selection_time, reports);
                // The exiting application asked us to save, and is gone: nothing to answer.
                if self.save.take().is_some() {
                    reports.push(Report::SaveAnswered { taken: kept });
                }
            }
        }
    }

    /// Claims CLIPBOARD for the candidate captured from the owner that has just died.
    fn keep_alive(
        &mut self,
        port: &mut impl Port,
        selection_time: Timestamp,
        reports: &mut Vec<Report>,
    ) -> bool {
        let Some(candidate) = self.candidate.take() else { return false };
        if candidate.selection_time != selection_time {
            return false;
        }
        // Claim with the dead owner's own claim time: any client that claimed after the death has
        // a later time, and the server prefers it (docs/selection.md, "Keep-alive").
        let clipboard = self.known.clipboard;
        port.set_selection_owner(clipboard, Some(self.our_window), selection_time);
        if port.selection_owner(clipboard) != Some(self.our_window) {
            return false;
        }
        let served = Served::new(port, Some(candidate.content), selection_time);
        self.ownership = Ownership::Ours(served);
        reports.push(Report::KeptAlive);
        true
    }

    fn start_capture(
        &mut self,
        port: &mut impl Port,
        owner: Window,
        selection_time: Timestamp,
        now: Millis,
        reports: &mut Vec<Report>,
    ) {
        self.ownership = Ownership::Theirs { selection_time };
        self.capture_seq = self.capture_seq.saturating_add(1);
        reports.push(Report::CaptureStarted { capture: self.capture_seq });
        self.capture_open = false;
        let mut cx = Ctx {
            port,
            known: &self.known,
            pool: &mut self.pool,
            our_window: self.our_window,
            limits: self.config.limits,
            now,
        };
        self.capture = Some(Capture::start(&mut cx, self.known.clipboard, owner, selection_time));
        self.probe.next_at = Some(now.after(PROBE_EVERY_MS));
    }

    /// Abandons the capture, telling the daemon if it had reported content not yet complete.
    fn end_capture(&mut self, now: Millis, reports: &mut Vec<Report>) {
        if let Some(mut c) = self.capture.take() {
            c.abandon(&mut self.pool, now);
            if self.capture_open {
                reports.push(Report::CaptureEnded { capture: self.capture_seq });
            }
        }
        self.capture_open = false;
    }

    /// Answers a pending `SAVE_TARGETS` with "not saved": the clipboard has moved on.
    fn abort_save(&mut self, port: &mut impl Port, reports: &mut Vec<Report>) {
        if let Some(save) = self.save.take() {
            let (manager, target) = (self.known.clipboard_manager, self.known.save_targets);
            port.send_selection_notify(save.requestor, manager, target, None, save.time);
            reports.push(Report::SaveAnswered { taken: false });
        }
    }

    /// Stops the liveness probe. Its property may still be answered, so it is quarantined.
    fn cancel_probe(&mut self, now: Millis) {
        if let Some((property, _)) = self.probe.outstanding.take() {
            self.pool.quarantine(property, now);
        }
        self.probe = Probe::default();
    }

    fn with_capture<P: Port>(
        &mut self,
        port: &mut P,
        now: Millis,
        step: impl FnOnce(&mut Capture, &mut Ctx<'_, P>) -> Progress,
    ) -> Progress {
        let Some(capture) = &mut self.capture else { return Progress::Nothing };
        let mut cx = Ctx {
            port,
            known: &self.known,
            pool: &mut self.pool,
            our_window: self.our_window,
            limits: self.config.limits,
            now,
        };
        step(capture, &mut cx)
    }

    fn progressed(&mut self, progress: Progress, reports: &mut Vec<Report>) {
        let Some(capture) = &self.capture else { return };
        match progress {
            Progress::Nothing => {}
            Progress::Content { content, complete } => {
                self.candidate = Some(Candidate {
                    selection_time: capture.selection_time,
                    content: Arc::clone(&content),
                });
                self.capture_open = !complete;
                reports.push(Report::Captured {
                    capture: self.capture_seq,
                    content,
                    source: capture.source.clone(),
                    complete,
                });
            }
            Progress::Failed => {
                self.capture_open = false;
                reports.push(Report::CaptureFailed { capture: self.capture_seq });
            }
        }
    }

    fn request(
        &mut self,
        port: &mut impl Port,
        req: &Request,
        now: Millis,
        reports: &mut Vec<Report>,
    ) {
        if req.selection == self.known.clipboard_manager && self.manager_owned {
            self.manager_request(port, req, now, reports);
            return;
        }
        let served = match &self.ownership {
            Ownership::Ours(served) if req.selection == self.known.clipboard => Some(served),
            Ownership::Ours(_) | Ownership::Theirs { .. } | Ownership::Unowned => {
                self.primary_cleared.as_ref().filter(|_| req.selection == self.known.primary)
            }
        };
        self.transfers.answer(port, &self.known, served, req, now);
    }

    /// `CLIPBOARD_MANAGER` requests: `TARGETS`, and `SAVE_TARGETS` from an exiting application
    /// (docs/selection.md, "`CLIPBOARD_MANAGER`").
    fn manager_request(
        &mut self,
        port: &mut impl Port,
        req: &Request,
        now: Millis,
        reports: &mut Vec<Report>,
    ) {
        let k = &self.known;
        if req.target == k.targets {
            let reply_property = req.property.unwrap_or(req.target);
            let list: Vec<u8> = [k.targets, k.save_targets, k.timestamp]
                .iter()
                .flat_map(|a| a.0.to_ne_bytes())
                .collect();
            port.change_property(req.requestor, reply_property, k.atom, Format::Bits32, &list);
            let reply = Some(reply_property);
            port.send_selection_notify(req.requestor, req.selection, req.target, reply, req.time);
            return;
        }
        if req.target != k.save_targets || self.save.is_some() {
            port.send_selection_notify(req.requestor, req.selection, req.target, None, req.time);
            return;
        }
        let reply_property = req.property.unwrap_or(k.save_targets);
        // Qt sends SAVE_TARGETS when it owns only PRIMARY: taking CLIPBOARD then would steal it
        // from a live application. Answer success and take nothing.
        if port.selection_owner(k.clipboard) != Some(req.requestor) {
            port.change_property(req.requestor, reply_property, k.null, Format::Bits32, &[]);
            let reply = Some(reply_property);
            port.send_selection_notify(req.requestor, req.selection, req.target, reply, req.time);
            reports.push(Report::SaveAnswered { taken: false });
            return;
        }
        // The requestor owns CLIPBOARD, so the capture of it, started when it claimed, is the one
        // to extend. Without one (it claimed before we started and we have not seen it yet), there
        // is nothing to save from.
        if self.capture.as_ref().is_none_or(|c| c.owner != req.requestor) {
            port.send_selection_notify(req.requestor, req.selection, req.target, None, req.time);
            reports.push(Report::SaveAnswered { taken: false });
            return;
        }
        let listed =
            req.property.map(|p| Self::read_atom_list(port, req.requestor, p)).unwrap_or_default();
        let all = listed.is_empty();
        let progress = self.with_capture(port, now, |c, cx| c.extend(cx, all, &listed));
        self.progressed(progress, reports);
        self.save = Some(Save {
            requestor: req.requestor,
            property: reply_property,
            time: req.time,
            deadline: now.after(SAVE_BUDGET_MS),
        });
        self.finish_save_if_ready(port, now, reports);
    }

    fn read_atom_list(
        port: &mut impl Port,
        window: Window,
        property: Atom,
    ) -> Vec<(Atom, TargetName)> {
        let Read::Value(list) = port.read_property(window, property, false, MAX_ATOM_LIST_BYTES)
        else {
            return Vec::new();
        };
        if list.format != Format::Bits32 {
            return Vec::new();
        }
        list.data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| Atom(u32::from_ne_bytes(*b)))
            .filter(|&a| a != Atom::NONE)
            .filter_map(|a| port.atom_name(a).map(|n| (a, n)))
            .collect()
    }

    fn finish_save_if_ready(
        &mut self,
        port: &mut impl Port,
        now: Millis,
        reports: &mut Vec<Report>,
    ) {
        if self.save.is_some() && self.capture.as_ref().is_none_or(Capture::is_done) {
            self.finish_save(port, now, reports);
        }
    }

    /// Answers the pending `SAVE_TARGETS`: takes the clipboard if anything was kept.
    fn finish_save(&mut self, port: &mut impl Port, now: Millis, reports: &mut Vec<Report>) {
        let Some(save) = self.save.take() else { return };
        let manager = self.known.clipboard_manager;
        let target = self.known.save_targets;
        let Some(candidate) = self.candidate.take() else {
            port.send_selection_notify(save.requestor, manager, target, None, save.time);
            reports.push(Report::SaveAnswered { taken: false });
            return;
        };
        let time =
            if save.time == Timestamp::CURRENT_TIME { port.server_time() } else { save.time };
        let clipboard = self.known.clipboard;
        port.set_selection_owner(clipboard, Some(self.our_window), time);
        let taken = port.selection_owner(clipboard) == Some(self.our_window);
        if taken {
            self.end_capture(now, reports);
            self.cancel_probe(now);
            self.ownership = Ownership::Ours(Served::new(port, Some(candidate.content), time));
            let null = self.known.null;
            port.change_property(save.requestor, save.property, null, Format::Bits32, &[]);
            let reply = Some(save.property);
            port.send_selection_notify(save.requestor, manager, target, reply, save.time);
        } else {
            self.candidate = Some(candidate);
            port.send_selection_notify(save.requestor, manager, target, None, save.time);
        }
        reports.push(Report::SaveAnswered { taken });
    }

    /// Whether this notification answered the liveness probe (and was consumed by it). The probe
    /// converts with the owner's selection time, so its answer must carry that time.
    fn probe_answered(
        &mut self,
        port: &mut impl Port,
        target: Atom,
        property: Option<Atom>,
        time: Timestamp,
        now: Millis,
    ) -> bool {
        let Some((ours, _)) = self.probe.outstanding else { return false };
        let Ownership::Theirs { selection_time } = self.ownership else { return false };
        if target != self.known.targets || !answers(ours, selection_time, property, time) {
            return false;
        }
        let incr = property.is_some_and(|_| {
            let read = port.read_property(self.our_window, ours, true, MAX_ATOM_LIST_BYTES);
            matches!(read, Read::Value(p) if p.type_ == self.known.incr)
        });
        if incr {
            self.pool.quarantine(ours, now);
        } else {
            self.pool.release(ours);
        }
        self.probe.outstanding = None;
        self.probe.misses = 0;
        true
    }

    fn probe_tick(&mut self, port: &mut impl Port, now: Millis, reports: &mut Vec<Report>) {
        let Ownership::Theirs { selection_time } = self.ownership else { return };
        if let Some((property, deadline)) = self.probe.outstanding {
            if now < deadline {
                return;
            }
            self.pool.quarantine(property, now);
            self.probe.outstanding = None;
            self.probe.misses = self.probe.misses.saturating_add(1);
            if self.probe.misses == PROBE_MISSES {
                reports.push(Report::OwnerHung);
            }
            self.probe.next_at = Some(now.after(PROBE_EVERY_MS));
            return;
        }
        if self.probe.next_at.is_some_and(|t| now < t) {
            return;
        }
        // A capture or a save is already talking to the owner; probe a period after it, not now.
        if self.capture.as_ref().is_some_and(|c| !c.is_done()) || self.save.is_some() {
            self.probe.next_at = Some(now.after(PROBE_EVERY_MS));
            return;
        }
        if let Some(property) = self.pool.take(port, now) {
            let (clipboard, targets) = (self.known.clipboard, self.known.targets);
            port.convert_selection(clipboard, targets, property, selection_time);
            self.probe.outstanding = Some((property, now.after(ANSWER_MS)));
        }
        self.probe.next_at = Some(now.after(PROBE_EVERY_MS));
    }
}
