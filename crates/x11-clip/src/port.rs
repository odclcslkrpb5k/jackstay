//! The selection engine's [`Port`] over a real X connection (ER-3: the one module that talks to
//! x11rb for the clipboard).

use std::collections::{HashMap, VecDeque};

use jackstay_clip::{Format, TargetName};
use jackstay_selection::{Atom, Port, Property, Read, Timestamp, Window};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::errors::{ConnectionError, ReplyError};
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::xproto::{
    self, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _, EventMask,
    PropMode, SelectionNotifyEvent,
};
use x11rb::rust_connection::RustConnection;

/// `ChangeProperty`'s fixed header, which the request-size limit includes.
const CHANGE_PROPERTY_HEADER: usize = 24;
/// The core protocol's largest request, 65535 words; larger needs BIG-REQUESTS.
const CORE_MAX_REQUEST_BYTES: usize = 65_535 * 4;
/// How much of `WM_CLASS` is read for attribution, in 32-bit units (256 bytes).
const WM_CLASS_WORDS: u32 = 64;

/// The X connection as the engine sees it. A broken connection is remembered, and every later call
/// answers emptily; the session checks `broken` after each turn and stops.
///
/// Its `Debug` leaves out the connection, whose buffers hold requests and replies carrying
/// clipboard contents.
pub(crate) struct X11Port {
    pub conn: RustConnection,
    pub root: xproto::Window,
    pub window: xproto::Window,
    /// The property on our window used to read the server's time; never one of the engine's.
    time_property: xproto::Atom,
    names: HashMap<u32, TargetName>,
    atoms: HashMap<TargetName, u32>,
    /// Events read while a query waited, to be handled next, in order.
    pub queued: VecDeque<XEvent>,
    pub broken: Option<ConnectionError>,
    max_property: usize,
}

impl std::fmt::Debug for X11Port {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11Port")
            .field("root", &self.root)
            .field("window", &self.window)
            .field("queued", &self.queued.len())
            .field("broken", &self.broken)
            .field("max_property", &self.max_property)
            .finish_non_exhaustive()
    }
}

/// Waits for a query's reply, consuming its cookie so the connection is free again.
fn ask<R: x11rb::x11_utils::TryParse>(
    cookie: Result<x11rb::cookie::Cookie<'_, RustConnection, R>, ConnectionError>,
) -> Result<Option<R>, ConnectionError> {
    match cookie?.reply() {
        Ok(r) => Ok(Some(r)),
        Err(ReplyError::X11Error(_)) => Ok(None),
        Err(ReplyError::ConnectionError(e)) => Err(e),
    }
}

impl X11Port {
    pub(crate) fn new(
        conn: RustConnection,
        root: xproto::Window,
        window: xproto::Window,
        time_property: xproto::Atom,
    ) -> Self {
        // With BIG-REQUESTS the request also carries a 4-byte extended length, which x11rb adds
        // after checking the size against the maximum.
        let max_request = conn.maximum_request_bytes();
        let header = if max_request > CORE_MAX_REQUEST_BYTES {
            CHANGE_PROPERTY_HEADER.saturating_add(4)
        } else {
            CHANGE_PROPERTY_HEADER
        };
        let max_property = max_request.saturating_sub(header);
        X11Port {
            conn,
            root,
            window,
            time_property,
            names: HashMap::new(),
            atoms: HashMap::new(),
            queued: VecDeque::new(),
            broken: None,
            max_property,
        }
    }

    /// Records a connection failure from a request that was only sent.
    fn sent<T>(&mut self, result: Result<T, ConnectionError>) {
        if let Err(e) = result
            && self.broken.is_none()
        {
            self.broken = Some(e);
        }
    }

    /// A query's answer: `None` for an X error (a window gone, say); a connection failure is
    /// remembered, then `None`.
    fn got<R>(&mut self, answer: Result<Option<R>, ConnectionError>) -> Option<R> {
        match answer {
            Ok(r) => r,
            Err(e) => {
                self.sent::<()>(Err(e));
                None
            }
        }
    }

    /// The class part of a window's `WM_CLASS` (`instance\0class\0`).
    fn wm_class(&mut self, window: xproto::Window) -> Option<String> {
        let cookie = ask(self.conn.get_property(
            false,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            0,
            WM_CLASS_WORDS,
        ));
        let reply = self.got(cookie)?;
        let class = reply.value.split(|&b| b == 0).nth(1).filter(|s| !s.is_empty())?;
        Some(class.iter().map(|&b| char::from(b)).collect())
    }

    /// A window-valued property (`WM_CLIENT_LEADER`, `_NET_ACTIVE_WINDOW`).
    fn window_property(
        &mut self,
        window: xproto::Window,
        property: &str,
    ) -> Option<xproto::Window> {
        let atom = self.intern_name(property)?;
        let cookie = ask(self.conn.get_property(false, window, atom, AtomEnum::WINDOW, 0, 1));
        self.got(cookie)?.value32()?.next().filter(|&w| w != x11rb::NONE)
    }

    fn intern_name(&mut self, name: &str) -> Option<u32> {
        let name = TargetName::from_latin1(name.as_bytes()).ok()?;
        Some(Port::intern(self, &name).0)
    }

    /// The name of the process behind `_NET_WM_PID`, only for a client on this machine: a remote
    /// client's pid (ssh -X) would name an unrelated local process.
    fn pid_name(&mut self, window: xproto::Window) -> Option<String> {
        let machine = self.intern_name("WM_CLIENT_MACHINE")?;
        let cookie = ask(self.conn.get_property(false, window, machine, AtomEnum::STRING, 0, 64));
        let host = self.got(cookie)?.value;
        let local = rustix::system::uname();
        if host.as_slice() != local.nodename().to_bytes() {
            return None;
        }
        let atom = self.intern_name("_NET_WM_PID")?;
        let cookie = ask(self.conn.get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1));
        let pid = self.got(cookie)?.value32()?.next()?;
        let comm = std::fs::read(format!("/proc/{pid}/comm")).ok()?;
        let name = String::from_utf8_lossy(comm.get(..comm.len().min(64))?).trim().to_owned();
        (!name.is_empty()).then_some(name)
    }
}

impl Port for X11Port {
    fn read_property(
        &mut self,
        window: Window,
        property: Atom,
        delete: bool,
        limit: usize,
    ) -> Read {
        // First the size and type alone, so a property past the limit is never transferred.
        let head = ask(self.conn.get_property(false, window.0, property.0, AtomEnum::ANY, 0, 0));
        let Some(head) = self.got(head) else { return Read::Missing };
        if head.type_ == x11rb::NONE {
            return Read::Missing;
        }
        let len = head.bytes_after;
        if usize::try_from(len).map_or(true, |l| l > limit) {
            return Read::TooLarge { len: u64::from(len) };
        }
        // Then all of it; X deletes it only if nothing remains after this read.
        let words = len.div_ceil(4);
        let all =
            ask(self.conn.get_property(delete, window.0, property.0, AtomEnum::ANY, 0, words));
        let Some(all) = self.got(all) else { return Read::Missing };
        let Ok(format) = Format::from_bits(all.format) else { return Read::Missing };
        if all.bytes_after != 0 || all.value.len() > limit {
            // It grew between the two reads: never hand back part of a value.
            return Read::TooLarge {
                len: u64::from(len).saturating_add(u64::from(all.bytes_after)),
            };
        }
        Read::Value(Property { type_: Atom(all.type_), format, data: all.value })
    }

    fn max_property_bytes(&self) -> usize {
        self.max_property
    }

    fn atom_name(&mut self, atom: Atom) -> Option<TargetName> {
        if let Some(name) = self.names.get(&atom.0) {
            return Some(name.clone());
        }
        let cookie = ask(self.conn.get_atom_name(atom.0));
        let name = TargetName::from_latin1(&self.got(cookie)?.name).ok()?;
        self.names.insert(atom.0, name.clone());
        self.atoms.insert(name.clone(), atom.0);
        Some(name)
    }

    fn intern(&mut self, name: &TargetName) -> Atom {
        if let Some(&atom) = self.atoms.get(name) {
            return Atom(atom);
        }
        let cookie = ask(self.conn.intern_atom(false, &name.to_latin1()));
        let Some(atom) = self.got(cookie).map(|r| r.atom) else { return Atom::NONE };
        self.atoms.insert(name.clone(), atom);
        self.names.insert(atom, name.clone());
        Atom(atom)
    }

    fn selection_owner(&mut self, selection: Atom) -> Option<Window> {
        let cookie = ask(self.conn.get_selection_owner(selection.0));
        self.got(cookie).map(|r| r.owner).filter(|&o| o != x11rb::NONE).map(Window)
    }

    fn server_time(&mut self) -> Timestamp {
        // A zero-length REPLACE never fails on a type or format another client gave the
        // property. Its PropertyNotify, and any error, carry the request's sequence number, so
        // neither a stranger's write to the property nor a lost window can be taken for, or
        // block, ours.
        let sent = self
            .conn
            .change_property(
                PropMode::REPLACE,
                self.window,
                self.time_property,
                AtomEnum::INTEGER,
                32,
                0,
                &[],
            )
            .map(|cookie| cookie.sequence_number());
        let sequence = match sent {
            Ok(sequence) => sequence,
            Err(e) => {
                self.sent::<()>(Err(e));
                return Timestamp::CURRENT_TIME;
            }
        };
        // Events and errors carry the low 16 bits of the sequence number.
        let ours = u16::try_from(sequence & 0xFFFF).unwrap_or(0);
        let flushed = self.conn.flush();
        self.sent(flushed);
        while self.broken.is_none() {
            match self.conn.wait_for_event() {
                Ok(XEvent::PropertyNotify(e))
                    if e.window == self.window
                        && e.atom == self.time_property
                        && e.sequence == ours =>
                {
                    return Timestamp(e.time);
                }
                Ok(XEvent::Error(e)) if e.sequence == ours => {
                    // Our own window or property refused: we cannot take part any more.
                    self.broken = Some(ConnectionError::UnknownError);
                }
                Ok(other) => self.queued.push_back(other),
                Err(e) => self.broken = Some(e),
            }
        }
        Timestamp::CURRENT_TIME
    }

    fn source_of(&mut self, owner: Window) -> Option<String> {
        if let Some(class) = self.wm_class(owner.0) {
            return Some(class);
        }
        if let Some(leader) = self.window_property(owner.0, "WM_CLIENT_LEADER")
            && let Some(class) = self.wm_class(leader)
        {
            return Some(class);
        }
        if let Some(name) = self.pid_name(owner.0) {
            return Some(name);
        }
        let active = self.window_property(self.root, "_NET_ACTIVE_WINDOW")?;
        self.wm_class(active)
    }

    fn convert_selection(
        &mut self,
        selection: Atom,
        target: Atom,
        property: Atom,
        time: Timestamp,
    ) {
        let r = self
            .conn
            .convert_selection(self.window, selection.0, target.0, property.0, time.0)
            .map(drop);
        self.sent(r);
    }

    fn change_property(
        &mut self,
        window: Window,
        property: Atom,
        type_: Atom,
        format: Format,
        data: &[u8],
    ) {
        let units = data.len() / format.unit_len();
        let Ok(units) = u32::try_from(units) else { return };
        let r = self
            .conn
            .change_property(
                PropMode::REPLACE,
                window.0,
                property.0,
                type_.0,
                format.bits(),
                units,
                data,
            )
            .map(drop);
        self.sent(r);
    }

    fn delete_property(&mut self, window: Window, property: Atom) {
        let r = self.conn.delete_property(window.0, property.0).map(drop);
        self.sent(r);
    }

    fn send_selection_notify(
        &mut self,
        requestor: Window,
        selection: Atom,
        target: Atom,
        property: Option<Atom>,
        time: Timestamp,
    ) {
        let event = SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: time.0,
            requestor: requestor.0,
            selection: selection.0,
            target: target.0,
            property: property.map_or(x11rb::NONE, |p| p.0),
        };
        let r = self.conn.send_event(false, requestor.0, EventMask::NO_EVENT, event).map(drop);
        self.sent(r);
    }

    fn set_selection_owner(&mut self, selection: Atom, owner: Option<Window>, time: Timestamp) {
        let owner = owner.map_or(x11rb::NONE, |w| w.0);
        let r = self.conn.set_selection_owner(owner, selection.0, time.0).map(drop);
        self.sent(r);
    }

    fn watch_window(&mut self, window: Window, watch: bool) {
        if window.0 == self.window {
            return;
        }
        let mask = if watch {
            EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY
        } else {
            EventMask::NO_EVENT
        };
        let aux = ChangeWindowAttributesAux::new().event_mask(mask);
        let r = self.conn.change_window_attributes(window.0, &aux).map(drop);
        self.sent(r);
    }

    fn announce_manager(&mut self, selection: Atom, time: Timestamp) {
        let Some(manager) = self.intern_name("MANAGER") else { return };
        let data = [time.0, selection.0, self.window, 0, 0];
        let event = ClientMessageEvent::new(32, self.root, manager, data);
        let r =
            self.conn.send_event(false, self.root, EventMask::STRUCTURE_NOTIFY, event).map(drop);
        self.sent(r);
    }

    fn clear_cut_buffers(&mut self) {
        let buffers = [
            AtomEnum::CUT_BUFFER0,
            AtomEnum::CUT_BUFFER1,
            AtomEnum::CUT_BUFFER2,
            AtomEnum::CUT_BUFFER3,
            AtomEnum::CUT_BUFFER4,
            AtomEnum::CUT_BUFFER5,
            AtomEnum::CUT_BUFFER6,
            AtomEnum::CUT_BUFFER7,
        ];
        for buffer in buffers {
            let r = self
                .conn
                .change_property(PropMode::REPLACE, self.root, buffer, AtomEnum::STRING, 8, 0, &[])
                .map(drop);
            self.sent(r);
        }
    }

    fn grab_server(&mut self, grab: bool) {
        let r = if grab { self.conn.grab_server() } else { self.conn.ungrab_server() }.map(drop);
        self.sent(r);
    }
}
