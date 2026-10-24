//! A scripted X client for what xclip cannot be made to do: own the clipboard with chosen window
//! properties, hang, destroy its window, take the clipboard from us.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

fn connect(display: &str) -> (RustConnection, u32) {
    let (conn, screen) = RustConnection::connect(Some(display)).unwrap_or_else(|e| panic!("{e}"));
    let root = conn.setup().roots[screen].root;
    (conn, root)
}

fn atom(conn: &RustConnection, name: &str) -> u32 {
    conn.intern_atom(false, name.as_bytes())
        .unwrap_or_else(|e| panic!("{e}"))
        .reply()
        .unwrap_or_else(|e| panic!("{e}"))
        .atom
}

/// How the owner identifies itself.
#[derive(Clone, Default)]
pub struct Identity {
    pub wm_class: Option<String>,
    /// `WM_CLASS` exactly as given, for malformed or unusual values.
    pub wm_class_raw: Option<Vec<u8>>,
    /// `WM_CLIENT_MACHINE`.
    pub machine: Option<String>,
    pub pid: Option<u32>,
    /// A separate leader window carrying this `WM_CLASS`, named by `WM_CLIENT_LEADER`.
    pub leader_class: Option<String>,
}

enum Order {
    DestroyWindow,
    Stop,
}

/// Owns CLIPBOARD on its own connection and thread, serving `text` (or never answering).
pub struct Owner {
    orders: Sender<Order>,
    /// Targets requested of it, in order.
    pub requests: Receiver<String>,
    thread: Option<JoinHandle<()>>,
}

impl Owner {
    pub fn start(display: &str, text: &'static str, identity: Identity, answers: bool) -> Self {
        let (orders, order_rx) = mpsc::channel();
        let (req_tx, requests) = mpsc::channel();
        let (ready_tx, ready) = mpsc::channel();
        let display = display.to_owned();
        let thread = std::thread::spawn(move || {
            serve(&display, text, &identity, answers, &order_rx, &req_tx, &ready_tx);
        });
        ready
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("owner not ready: {e}"));
        Owner { orders, requests, thread: Some(thread) }
    }

    pub fn destroy_window(&self) {
        let _ = self.orders.send(Order::DestroyWindow);
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.orders.send(Order::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(
    display: &str,
    text: &str,
    identity: &Identity,
    answers: bool,
    orders: &Receiver<Order>,
    requests: &Sender<String>,
    ready: &Sender<()>,
) {
    let (conn, root) = connect(display);
    let window = conn.generate_id().unwrap_or_else(|e| panic!("{e}"));
    let aux = CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE);
    let _ = conn.create_window(0, window, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &aux);
    identify(&conn, root, window, identity);
    let time = server_time(&conn, window);
    let clipboard = atom(&conn, "CLIPBOARD");
    let (targets, utf8) = (atom(&conn, "TARGETS"), atom(&conn, "UTF8_STRING"));
    let _ = conn.set_selection_owner(window, clipboard, time);
    let _ = conn.flush();
    let _ = ready.send(());
    loop {
        match orders.try_recv() {
            Ok(Order::DestroyWindow) => {
                let _ = conn.destroy_window(window);
                let _ = conn.flush();
            }
            Ok(Order::Stop) | Err(mpsc::TryRecvError::Disconnected) => return,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let Ok(event) = conn.poll_for_event() else { return };
        let Some(Event::SelectionRequest(r)) = event else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        let name = conn.get_atom_name(r.target).ok().and_then(|c| c.reply().ok());
        let _ = requests
            .send(name.map(|n| String::from_utf8_lossy(&n.name).into_owned()).unwrap_or_default());
        if !answers {
            continue;
        }
        let property = if r.target == targets {
            let _ = conn.change_property32(
                PropMode::REPLACE,
                r.requestor,
                r.property,
                AtomEnum::ATOM,
                &[targets, utf8],
            );
            r.property
        } else if r.target == utf8 {
            let _ = conn.change_property8(
                PropMode::REPLACE,
                r.requestor,
                r.property,
                utf8,
                text.as_bytes(),
            );
            r.property
        } else {
            x11rb::NONE
        };
        let notify = SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: r.time,
            requestor: r.requestor,
            selection: r.selection,
            target: r.target,
            property,
        };
        let _ = conn.send_event(false, r.requestor, EventMask::NO_EVENT, notify);
        let _ = conn.flush();
    }
}

/// Sets the window properties an owner is attributed by.
fn identify(conn: &RustConnection, root: u32, window: u32, identity: &Identity) {
    if let Some(class) = &identity.wm_class {
        let value = format!("{}\0{class}\0", class.to_lowercase());
        let _ = conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            value.as_bytes(),
        );
    }
    if let Some(raw) = &identity.wm_class_raw {
        let _ = conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            raw,
        );
    }
    if let Some(machine) = &identity.machine {
        let a = atom(conn, "WM_CLIENT_MACHINE");
        let _ = conn.change_property8(
            PropMode::REPLACE,
            window,
            a,
            AtomEnum::STRING,
            machine.as_bytes(),
        );
    }
    if let Some(pid) = identity.pid {
        let a = atom(conn, "_NET_WM_PID");
        let _ = conn.change_property32(PropMode::REPLACE, window, a, AtomEnum::CARDINAL, &[pid]);
    }
    if let Some(class) = &identity.leader_class {
        let leader = conn.generate_id().unwrap_or_else(|e| panic!("{e}"));
        let _ = conn.create_window(
            0,
            leader,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        );
        let value = format!("{}\0{class}\0", class.to_lowercase());
        let _ = conn.change_property8(
            PropMode::REPLACE,
            leader,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            value.as_bytes(),
        );
        let a = atom(conn, "WM_CLIENT_LEADER");
        let _ = conn.change_property32(PropMode::REPLACE, window, a, AtomEnum::WINDOW, &[leader]);
    }
}

/// A real timestamp, from a zero-length append.
fn server_time(conn: &RustConnection, window: u32) -> u32 {
    // A real timestamp for the claim, from a zero-length append.
    let stamp = atom(conn, "OWNER_TIME");
    let _ = conn.change_property8(PropMode::APPEND, window, stamp, AtomEnum::STRING, &[]);
    let _ = conn.flush();
    loop {
        if let Ok(Event::PropertyNotify(e)) = conn.wait_for_event()
            && e.atom == stamp
        {
            return e.time;
        }
    }
}

/// The current owner of a selection.
pub fn owner_of(display: &str, selection: &str) -> u32 {
    let (conn, _) = connect(display);
    let s = atom(&conn, selection);
    conn.get_selection_owner(s).ok().and_then(|c| c.reply().ok()).map_or(0, |r| r.owner)
}

/// A window's `WM_CLASS` class.
pub fn class_of(display: &str, window: u32) -> Option<String> {
    let (conn, _) = connect(display);
    let r = conn
        .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 64)
        .ok()?
        .reply()
        .ok()?;
    let class = r.value.split(|&b| b == 0).nth(1)?;
    Some(String::from_utf8_lossy(class).into_owned())
}

pub fn set_cut_buffer(display: &str, text: &[u8]) {
    let (conn, root) = connect(display);
    let _ = conn.change_property8(
        PropMode::REPLACE,
        root,
        AtomEnum::CUT_BUFFER0,
        AtomEnum::STRING,
        text,
    );
    let _ = conn.sync();
}

pub fn cut_buffer(display: &str) -> Vec<u8> {
    let (conn, root) = connect(display);
    conn.get_property(false, root, AtomEnum::CUT_BUFFER0, AtomEnum::ANY, 0, 1024)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.value)
        .unwrap_or_default()
}

/// Names of the properties on `window` that hold data.
pub fn properties_of(display: &str, window: u32) -> Vec<String> {
    let (conn, _) = connect(display);
    let Some(list) = conn.list_properties(window).ok().and_then(|c| c.reply().ok()) else {
        return Vec::new();
    };
    list.atoms
        .iter()
        .filter_map(|&a| conn.get_atom_name(a).ok()?.reply().ok())
        .map(|n| String::from_utf8_lossy(&n.name).into_owned())
        .collect()
}

/// Asks for CLIPBOARD as `UTF8_STRING`, reads only the `SelectionNotify` (an INCR reply leaves the
/// transfer waiting on us), then destroys its window without reading anything.
pub fn request_then_vanish(display: &str) -> String {
    let (conn, root) = connect(display);
    let window = conn.generate_id().unwrap_or_else(|e| panic!("{e}"));
    let aux = CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE);
    let _ = conn.create_window(0, window, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &aux);
    let (clipboard, utf8, prop) =
        (atom(&conn, "CLIPBOARD"), atom(&conn, "UTF8_STRING"), atom(&conn, "DATA"));
    let _ = conn.convert_selection(window, clipboard, utf8, prop, x11rb::CURRENT_TIME);
    let _ = conn.flush();
    let type_name = loop {
        match conn.wait_for_event() {
            Ok(Event::SelectionNotify(n)) => {
                let reply = conn
                    .get_property(false, window, prop, AtomEnum::ANY, 0, 0)
                    .ok()
                    .and_then(|c| c.reply().ok());
                let name = reply.and_then(|r| conn.get_atom_name(r.type_).ok()?.reply().ok());
                let _ = n;
                break name
                    .map(|n| String::from_utf8_lossy(&n.name).into_owned())
                    .unwrap_or_default();
            }
            Ok(_) => {}
            Err(e) => panic!("{e}"),
        }
    };
    let _ = conn.destroy_window(window);
    let _ = conn.sync();
    type_name
}

/// Listens on the root window for client messages, from before a session starts.
pub struct RootListener {
    conn: RustConnection,
}

impl RootListener {
    pub fn start(display: &str) -> Self {
        let (conn, root) = connect(display);
        let aux = x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
            .event_mask(EventMask::STRUCTURE_NOTIFY);
        let _ = conn.change_window_attributes(root, &aux);
        let _ = conn.sync();
        RootListener { conn }
    }

    /// The next `MANAGER` client message's data, within five seconds.
    pub fn manager_message(&self) -> Option<[u32; 5]> {
        let manager = atom(&self.conn, "MANAGER");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match self.conn.poll_for_event() {
                Ok(Some(Event::ClientMessage(m))) if m.type_ == manager => {
                    return Some(m.data.as_data32());
                }
                Ok(Some(_)) => {}
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => return None,
            }
        }
        None
    }

    pub fn atom(&self, name: &str) -> u32 {
        atom(&self.conn, name)
    }
}

/// Sets a property on another client's window, as any client may.
pub fn put_string(display: &str, window: u32, property: &str, value: &[u8]) {
    let (conn, _) = connect(display);
    let a = atom(&conn, property);
    let _ = conn.change_property8(PropMode::REPLACE, window, a, AtomEnum::STRING, value);
    let _ = conn.sync();
}

/// Destroys another client's window, as any client may.
pub fn destroy(display: &str, window: u32) {
    let (conn, _) = connect(display);
    let _ = conn.destroy_window(window);
    let _ = conn.sync();
}

/// This machine's name, as `WM_CLIENT_MACHINE` would give it.
pub fn hostname() -> String {
    rustix::system::uname().nodename().to_string_lossy().into_owned()
}
