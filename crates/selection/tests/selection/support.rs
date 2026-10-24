//! A fake X server behind the engine's port: atoms, window properties, selection ownership with
//! the server's time rule (dix/selection.c), and a log of every request the engine sends.

use std::collections::HashMap;
use std::sync::Arc;

use jackstay_clip::{Content, Format, Kept, Target, TargetData, TargetName};
use jackstay_selection::{
    Atom, Config, Engine, Event, Limits, Millis, OwnerChange, Port, Property, PropertyState, Read,
    Report, Timestamp, Window,
};

pub const OURS: Window = Window(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Convert {
        selection: Atom,
        target: Atom,
        property: Atom,
        time: Timestamp,
    },
    Change {
        window: Window,
        property: Atom,
        type_: Atom,
        format: Format,
        data: Vec<u8>,
    },
    Delete {
        window: Window,
        property: Atom,
    },
    Notify {
        requestor: Window,
        selection: Atom,
        target: Atom,
        property: Option<Atom>,
        time: Timestamp,
    },
    SetOwner {
        selection: Atom,
        owner: Option<Window>,
        time: Timestamp,
    },
    Watch {
        window: Window,
        on: bool,
    },
    Announce,
    ClearCutBuffers,
    Grab(bool),
}

#[derive(Debug, Default)]
pub struct FakeX {
    atoms: HashMap<String, Atom>,
    names: HashMap<Atom, String>,
    pub props: HashMap<(Window, Atom), Property>,
    /// selection → (owner, lastTimeChanged)
    pub owners: HashMap<Atom, (Option<Window>, Timestamp)>,
    pub time: u32,
    pub calls: Vec<Call>,
    pub sources: HashMap<Window, String>,
    /// The time of the latest conversion of each target, which owners echo in their replies.
    pub convert_times: HashMap<Atom, Timestamp>,
    /// The server's limit on one `ChangeProperty`.
    pub max_property: usize,
    pub grabbed: bool,
    /// A client about to claim CLIPBOARD at `CurrentTime`: its request is processed just before
    /// our next `SetSelectionOwner`, or at the ungrab if the server is grabbed then.
    pub racing_claim: Option<Window>,
}

impl FakeX {
    pub fn new() -> Self {
        FakeX { time: 1_000, max_property: 16 << 20, ..FakeX::default() }
    }

    /// The atom for `name`, interning it.
    pub fn a(&mut self, name: &str) -> Atom {
        if let Some(&atom) = self.atoms.get(name) {
            return atom;
        }
        let atom = Atom(u32::try_from(self.atoms.len()).unwrap_or(0) + 100);
        self.atoms.insert(name.to_owned(), atom);
        self.names.insert(atom, name.to_owned());
        atom
    }

    pub fn name_of(&self, atom: Atom) -> String {
        self.names.get(&atom).cloned().unwrap_or_else(|| format!("#{}", atom.0))
    }

    pub fn atom_list(&mut self, names: &[&str]) -> Vec<u8> {
        names.iter().flat_map(|n| self.a(n).0.to_ne_bytes()).collect()
    }

    /// Sets a property, as a client would.
    pub fn put(
        &mut self,
        window: Window,
        property: Atom,
        type_: &str,
        format: Format,
        data: &[u8],
    ) {
        let type_ = self.a(type_);
        self.props.insert((window, property), Property { type_, format, data: data.to_vec() });
    }

    /// Another client claims a selection, as the server would allow it.
    pub fn claim(&mut self, selection: &str, owner: Window, time: u32) {
        let selection = self.a(selection);
        self.owners.insert(selection, (Some(owner), Timestamp(time)));
    }

    /// Another client claims CLIPBOARD at `time`. The server applies its rule (a claim earlier than
    /// the last change, or later than now, is ignored); if the claim stands, the events the server
    /// would send follow: `SelectionClear` to us if we owned it, then the `XFixes` notification.
    pub fn other_claims(&mut self, owner: Window, time: u32) -> Vec<Event> {
        let clipboard = self.a("CLIPBOARD");
        let time = Timestamp(time);
        let previous = self.owners.get(&clipboard).copied();
        if time.0 > self.time || previous.is_some_and(|(_, last)| time.is_earlier_than(last)) {
            return Vec::new();
        }
        self.owners.insert(clipboard, (Some(owner), time));
        let mut events = Vec::new();
        if previous.is_some_and(|(o, _)| o == Some(OURS)) {
            events.push(Event::SelectionClear { selection: clipboard, time });
        }
        events.push(Event::OwnerChanged {
            selection: clipboard,
            owner: Some(owner),
            change: OwnerChange::Set,
            timestamp: Timestamp(self.time),
            selection_timestamp: time,
        });
        events
    }

    /// CLIPBOARD's owner dies, if it is another client: the server keeps `lastTimeChanged`.
    pub fn owner_dies(&mut self) -> Vec<Event> {
        let clipboard = self.a("CLIPBOARD");
        match self.owners.get(&clipboard).copied() {
            Some((Some(owner), last)) if owner != OURS => {
                self.owners.insert(clipboard, (None, last));
                vec![Event::OwnerChanged {
                    selection: clipboard,
                    owner: None,
                    change: OwnerChange::ClientClosed,
                    timestamp: Timestamp(self.time),
                    selection_timestamp: last,
                }]
            }
            _ => Vec::new(),
        }
    }

    pub fn take_calls(&mut self) -> Vec<Call> {
        std::mem::take(&mut self.calls)
    }

    /// The conversions asked for since the log was last taken, as (target name, property).
    pub fn converts(&mut self) -> Vec<(String, Atom)> {
        let calls = self.take_calls();
        calls
            .iter()
            .filter_map(|c| match c {
                Call::Convert { target, property, .. } => Some((self.name_of(*target), *property)),
                _ => None,
            })
            .collect()
    }
}

impl Port for FakeX {
    fn read_property(
        &mut self,
        window: Window,
        property: Atom,
        delete: bool,
        limit: usize,
    ) -> Read {
        match self.props.get(&(window, property)) {
            None => Read::Missing,
            Some(p) if p.data.len() > limit => Read::TooLarge { len: p.data.len() as u64 },
            Some(p) => {
                let p = p.clone();
                if delete {
                    self.props.remove(&(window, property));
                }
                Read::Value(p)
            }
        }
    }

    fn max_property_bytes(&self) -> usize {
        self.max_property
    }

    fn atom_name(&mut self, atom: Atom) -> Option<TargetName> {
        self.names.get(&atom).and_then(|n| TargetName::from_latin1(n.as_bytes()).ok())
    }

    fn intern(&mut self, name: &TargetName) -> Atom {
        self.a(name.as_str())
    }

    fn selection_owner(&mut self, selection: Atom) -> Option<Window> {
        self.owners.get(&selection).and_then(|o| o.0)
    }

    fn server_time(&mut self) -> Timestamp {
        self.time += 1;
        Timestamp(self.time)
    }

    fn source_of(&mut self, owner: Window) -> Option<String> {
        self.sources.get(&owner).cloned()
    }

    fn convert_selection(
        &mut self,
        selection: Atom,
        target: Atom,
        property: Atom,
        time: Timestamp,
    ) {
        self.convert_times.insert(target, time);
        self.calls.push(Call::Convert { selection, target, property, time });
    }

    fn change_property(
        &mut self,
        window: Window,
        property: Atom,
        type_: Atom,
        format: Format,
        data: &[u8],
    ) {
        self.props.insert((window, property), Property { type_, format, data: data.to_vec() });
        self.calls.push(Call::Change { window, property, type_, format, data: data.to_vec() });
    }

    fn delete_property(&mut self, window: Window, property: Atom) {
        self.props.remove(&(window, property));
        self.calls.push(Call::Delete { window, property });
    }

    fn send_selection_notify(
        &mut self,
        requestor: Window,
        selection: Atom,
        target: Atom,
        property: Option<Atom>,
        time: Timestamp,
    ) {
        self.calls.push(Call::Notify { requestor, selection, target, property, time });
    }

    /// The server's rule: a claim earlier than the selection's last change, or later than now, is
    /// ignored (dix/selection.c).
    fn set_selection_owner(&mut self, selection: Atom, owner: Option<Window>, time: Timestamp) {
        self.calls.push(Call::SetOwner { selection, owner, time });
        if !self.grabbed {
            self.race();
        }
        let time = if time == Timestamp::CURRENT_TIME { Timestamp(self.time) } else { time };
        if time.0 > self.time {
            return;
        }
        if let Some((_, last)) = self.owners.get(&selection)
            && time.is_earlier_than(*last)
        {
            return;
        }
        self.owners.insert(selection, (owner, time));
    }

    fn watch_window(&mut self, window: Window, watch: bool) {
        self.calls.push(Call::Watch { window, on: watch });
    }

    fn announce_manager(&mut self, _selection: Atom, _time: Timestamp) {
        self.calls.push(Call::Announce);
    }

    fn clear_cut_buffers(&mut self) {
        self.calls.push(Call::ClearCutBuffers);
    }

    fn grab_server(&mut self, grab: bool) {
        self.calls.push(Call::Grab(grab));
        self.grabbed = grab;
        if !grab {
            self.race();
        }
    }
}

impl FakeX {
    fn race(&mut self) {
        if let Some(owner) = self.racing_claim.take() {
            let clipboard = self.a("CLIPBOARD");
            self.owners.insert(clipboard, (Some(owner), Timestamp(self.time)));
        }
    }
}

pub fn limits() -> Limits {
    Limits { per_target: 1 << 20, per_entry: 4 << 20 }
}

pub fn engine(x: &mut FakeX) -> (Engine, Vec<Report>) {
    Engine::new(x, OURS, Config { limits: limits() }, Millis(0))
}

pub fn name(s: &str) -> TargetName {
    match TargetName::from_latin1(s.as_bytes()) {
        Ok(n) => n,
        Err(e) => panic!("bad name {s}: {e}"),
    }
}

pub fn text_content(s: &str) -> Arc<Content> {
    let kept = match Kept::new(name("UTF8_STRING"), Format::Bits8, s.as_bytes().to_vec()) {
        Ok(k) => k,
        Err(e) => panic!("{e}"),
    };
    let target = Target { name: name("UTF8_STRING"), data: TargetData::Kept(kept) };
    match Content::new(vec![target]) {
        Ok(c) => Arc::new(c),
        Err(e) => panic!("{e}"),
    }
}

pub fn set(x: &mut FakeX, owner: Option<Window>, time: u32) -> Event {
    if let Some(o) = owner {
        x.claim("CLIPBOARD", o, time);
    }
    Event::OwnerChanged {
        selection: x.a("CLIPBOARD"),
        owner,
        change: OwnerChange::Set,
        timestamp: Timestamp(time),
        selection_timestamp: Timestamp(time),
    }
}

/// The owner of CLIPBOARD died; the server leaves lastTimeChanged alone and reports owner None.
pub fn died(x: &mut FakeX, selection_time: u32, now: u32) -> Event {
    let clipboard = x.a("CLIPBOARD");
    if let Some(entry) = x.owners.get_mut(&clipboard) {
        entry.0 = None;
    }
    Event::OwnerChanged {
        selection: clipboard,
        owner: None,
        change: OwnerChange::ClientClosed,
        timestamp: Timestamp(now),
        selection_timestamp: Timestamp(selection_time),
    }
}

/// The owner answers the latest conversion of `target`: writes `data` to our property and
/// notifies, echoing the conversion's time as owners do.
pub fn answer(
    x: &mut FakeX,
    target: &str,
    property: Atom,
    type_: &str,
    format: Format,
    data: &[u8],
) -> Event {
    let target_atom = x.a(target);
    let time = x.convert_times.get(&target_atom).copied().unwrap_or(Timestamp(0));
    x.put(OURS, property, type_, format, data);
    Event::SelectionNotify {
        selection: x.a("CLIPBOARD"),
        target: x.a(target),
        property: Some(property),
        time,
    }
}

/// The owner refuses the latest conversion of `target`, echoing its time.
pub fn refuse(x: &mut FakeX, target: &str) -> Event {
    let target_atom = x.a(target);
    let time = x.convert_times.get(&target_atom).copied().unwrap_or(Timestamp(0));
    refuse_at(x, target, time)
}

/// A refusal of `target` carrying `time`, which may not be the latest conversion's.
pub fn refuse_at(x: &mut FakeX, target: &str, time: Timestamp) -> Event {
    Event::SelectionNotify {
        selection: x.a("CLIPBOARD"),
        target: x.a(target),
        property: None,
        time,
    }
}

pub fn request(x: &mut FakeX, requestor: Window, target: &str, property: &str, time: u32) -> Event {
    Event::SelectionRequest {
        requestor,
        selection: x.a("CLIPBOARD"),
        target: x.a(target),
        property: Some(x.a(property)),
        time: Timestamp(time),
    }
}

pub fn new_value(property: Atom) -> Event {
    Event::PropertyNotify {
        window: OURS,
        atom: property,
        state: PropertyState::NewValue,
        time: Timestamp(0),
    }
}

pub fn deleted(window: Window, property: Atom) -> Event {
    Event::PropertyNotify {
        window,
        atom: property,
        state: PropertyState::Deleted,
        time: Timestamp(0),
    }
}

/// The last report of captured content, if any.
pub fn captured(reports: &[Report]) -> Option<(Arc<Content>, bool)> {
    reports.iter().rev().find_map(|r| match r {
        Report::Captured { content, complete, .. } => Some((Arc::clone(content), *complete)),
        _ => None,
    })
}

pub fn kept_text(content: &Content, target: &str) -> Option<String> {
    content.kept(target).map(|k| String::from_utf8_lossy(k.bytes()).into_owned())
}

pub const FIREFOX: Window = Window(500);
pub const QT_APP: Window = Window(600);
pub const XTERM: Window = Window(700);

pub fn some<T>(o: Option<T>) -> T {
    match o {
        Some(v) => v,
        None => panic!("expected Some"),
    }
}

pub fn one<T: std::fmt::Debug>(mut v: Vec<T>) -> T {
    assert_eq!(v.len(), 1, "expected exactly one: {v:?}");
    some(v.pop())
}

/// Firefox copies `text`: `TARGETS`, then `UTF8_STRING`. Returns the engine with the capture complete.
pub fn firefox_copies(x: &mut FakeX, text: &str, time: u32) -> jackstay_selection::Engine {
    let (mut e, _) = engine(x);
    x.take_calls();
    send!(e, x, set(x, Some(FIREFOX), time), Millis(0));
    let (_, targets_prop) = one(x.converts());
    let list = x.atom_list(&["TARGETS", "UTF8_STRING", "TIMESTAMP"]);
    send!(e, x, answer(x, "TARGETS", targets_prop, "ATOM", Format::Bits32, &list), Millis(1));
    let (_, text_prop) = one(x.converts());
    let r = send!(
        e,
        x,
        answer(x, "UTF8_STRING", text_prop, "UTF8_STRING", Format::Bits8, text.as_bytes()),
        Millis(2),
    );
    let (content, complete) = some(captured(&r));
    assert!(complete);
    assert_eq!(kept_text(&content, "UTF8_STRING").as_deref(), Some(text));
    e
}

pub fn owning(x: &mut FakeX, text: &str) -> jackstay_selection::Engine {
    let (mut e, _) = engine(x);
    x.time = 2_000;
    assert_eq!(e.serve(x, text_content(text), Millis(0)), vec![Report::Claimed]);
    x.take_calls();
    e
}

/// The last `SelectionNotify` the engine sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    Refused,
    Sent(Atom),
}

pub fn notified(x: &mut FakeX) -> Option<Notice> {
    x.take_calls().iter().rev().find_map(|c| match c {
        Call::Notify { property: Some(p), .. } => Some(Notice::Sent(*p)),
        Call::Notify { property: None, .. } => Some(Notice::Refused),
        _ => None,
    })
}

pub fn written(x: &mut FakeX, window: Window, property: &str) -> Option<(String, Vec<u8>)> {
    let p = x.a(property);
    let prop = x.props.get(&(window, p))?.clone();
    Some((x.name_of(prop.type_), prop.data))
}

/// An owner's reply carrying `time`, which may not be the conversion's.
pub fn answer_at(
    x: &mut FakeX,
    target: &str,
    property: Atom,
    type_: &str,
    format: Format,
    data: &[u8],
    time: Timestamp,
) -> Event {
    x.put(OURS, property, type_, format, data);
    Event::SelectionNotify {
        selection: x.a("CLIPBOARD"),
        target: x.a(target),
        property: Some(property),
        time,
    }
}
