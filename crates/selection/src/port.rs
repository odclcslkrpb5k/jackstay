use std::fmt;

use jackstay_clip::{Format, TargetName};

use crate::{Atom, Timestamp, Window};

/// A property's value, as read. Its `Debug` shows the data's length, never the data.
#[derive(Clone, PartialEq, Eq)]
pub struct Property {
    pub type_: Atom,
    pub format: Format,
    pub data: Vec<u8>,
}

impl fmt::Debug for Property {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Property")
            .field("type_", &self.type_)
            .field("format", &self.format)
            .field("len", &self.data.len())
            .finish()
    }
}

/// The result of reading a property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    Missing,
    Value(Property),
    /// Larger than the limit asked for; nothing was read, and nothing was deleted.
    TooLarge {
        len: u64,
    },
}

/// What the engine needs from the X server. The adapter implements it over a real connection;
/// tests implement it with a scripted fake. Queries wait for their answer; requests are sent and
/// not waited for, and their X errors are the adapter's to log (docs/selection.md, "What the
/// adapter guarantees").
///
/// The contract, beyond each method's own:
/// - Format-16 and format-32 data, read or written, is 2 or 4 bytes per item in this client's
///   byte order (as xcb delivers it), never Xlib's `long` per item.
/// - Byte limits are in bytes and inclusive.
/// - Events that arrive while a query waits are queued and delivered afterwards, in server order.
/// - Queries answer with the server's state *now*, while the events being handled may be older.
///   The engine judges events by their timestamps for that reason; the adapter must not reorder or
///   drop them.
/// - `server_time` uses a property outside the engine's pool (`JACKSTAY_P*`), and its own
///   `PropertyNotify` is consumed by the adapter, not delivered.
/// - `watch_window(_, false)` never changes the event mask of our own window.
pub trait Port {
    /// Reads a whole property of up to `limit` bytes. With `delete`, deletes it once wholly read.
    fn read_property(&mut self, window: Window, property: Atom, delete: bool, limit: usize)
    -> Read;
    /// The most data one `ChangeProperty` may carry: the server's maximum request length (with
    /// BIG-REQUESTS if the server has it), less the request's header.
    fn max_property_bytes(&self) -> usize;
    fn atom_name(&mut self, atom: Atom) -> Option<TargetName>;
    fn intern(&mut self, name: &TargetName) -> Atom;
    fn selection_owner(&mut self, selection: Atom) -> Option<Window>;
    /// A current server timestamp (a zero-length append to a property on our window, and the
    /// time of its `PropertyNotify`).
    fn server_time(&mut self) -> Timestamp;
    /// Who an owner window belongs to (its `WM_CLASS`, its client leader's, the process, or the
    /// active window's), for an entry's source.
    fn source_of(&mut self, owner: Window) -> Option<String>;

    fn convert_selection(&mut self, selection: Atom, target: Atom, property: Atom, time: Timestamp);
    fn change_property(
        &mut self,
        window: Window,
        property: Atom,
        type_: Atom,
        format: Format,
        data: &[u8],
    );
    fn delete_property(&mut self, window: Window, property: Atom);
    fn send_selection_notify(
        &mut self,
        requestor: Window,
        selection: Atom,
        target: Atom,
        property: Option<Atom>,
        time: Timestamp,
    );
    /// Makes our window the owner (`Some`) or releases it (`None`).
    fn set_selection_owner(&mut self, selection: Atom, owner: Option<Window>, time: Timestamp);
    /// Starts or stops receiving `PropertyNotify` and `DestroyNotify` for another client's window.
    fn watch_window(&mut self, window: Window, watch: bool);
    /// Sends the ICCCM `MANAGER` client message for `CLIPBOARD_MANAGER` to the root window.
    fn announce_manager(&mut self, selection: Atom, time: Timestamp);
    /// Empties `CUT_BUFFER0` to `CUT_BUFFER7` on the root window.
    fn clear_cut_buffers(&mut self);
    /// Grabs the server (`true`), so no other client's request is processed until it is ungrabbed
    /// (`false`). Held only across a check and a claim that must not be split.
    fn grab_server(&mut self, grab: bool);
}
