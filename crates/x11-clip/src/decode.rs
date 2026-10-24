//! X events, as the selection engine sees them.

use jackstay_selection::{Atom, Event, OwnerChange, PropertyState, Timestamp, Window};
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::xfixes::SelectionEvent;
use x11rb::protocol::xproto::Property;

fn some_atom(atom: u32) -> Option<Atom> {
    (atom != x11rb::NONE).then_some(Atom(atom))
}

/// The engine's view of an X event, or `None` for events it does not take. X errors from requests
/// whose replies nobody waits for (a requestor's window gone mid-transfer) arrive here too, and are
/// dropped: the engine's deadlines and `DestroyNotify` clean up after them.
pub(crate) fn event(event: &XEvent) -> Option<Event> {
    Some(match event {
        XEvent::XfixesSelectionNotify(e) => Event::OwnerChanged {
            selection: Atom(e.selection),
            owner: (e.owner != x11rb::NONE).then_some(Window(e.owner)),
            change: match e.subtype {
                SelectionEvent::SELECTION_WINDOW_DESTROY => OwnerChange::WindowDestroyed,
                SelectionEvent::SELECTION_CLIENT_CLOSE => OwnerChange::ClientClosed,
                _ => OwnerChange::Set,
            },
            timestamp: Timestamp(e.timestamp),
            selection_timestamp: Timestamp(e.selection_timestamp),
        },
        XEvent::SelectionNotify(e) => Event::SelectionNotify {
            selection: Atom(e.selection),
            target: Atom(e.target),
            property: some_atom(e.property),
            time: Timestamp(e.time),
        },
        XEvent::SelectionRequest(e) => Event::SelectionRequest {
            requestor: Window(e.requestor),
            selection: Atom(e.selection),
            target: Atom(e.target),
            property: some_atom(e.property),
            time: Timestamp(e.time),
        },
        XEvent::SelectionClear(e) => {
            Event::SelectionClear { selection: Atom(e.selection), time: Timestamp(e.time) }
        }
        XEvent::PropertyNotify(e) => Event::PropertyNotify {
            window: Window(e.window),
            atom: Atom(e.atom),
            state: if e.state == Property::DELETE {
                PropertyState::Deleted
            } else {
                PropertyState::NewValue
            },
            time: Timestamp(e.time),
        },
        XEvent::DestroyNotify(e) => Event::DestroyNotify { window: Window(e.window) },
        _ => return None,
    })
}
