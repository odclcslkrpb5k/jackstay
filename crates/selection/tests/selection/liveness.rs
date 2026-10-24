//! "The engine cannot wedge" (docs/selection.md, "Liveness"), as a property: under any sequence of
//! events, replies, garbage and time, ticking at the engine's own deadline always moves that
//! deadline later, so the adapter's loop can neither spin nor stall.

use crate::support::{FakeX, OURS, engine, text_content};
use jackstay_clip::Format;
use jackstay_selection::{Atom, Event, Millis, OwnerChange, PropertyState, Timestamp, Window};
use proptest::prelude::*;

const NAMES: [&str; 13] = [
    "CLIPBOARD",
    "CLIPBOARD_MANAGER",
    "TARGETS",
    "UTF8_STRING",
    "STRING",
    "image/png",
    "text/html",
    "DELETE",
    "MULTIPLE",
    "SAVE_TARGETS",
    "INCR",
    "ATOM",
    "PRIMARY",
];

#[derive(Debug, Clone)]
enum Step {
    Owner {
        name: usize,
        owner: Option<u8>,
        change: u8,
        time: u16,
    },
    /// An owner writes something to one of our properties and notifies.
    Reply {
        target: usize,
        prop: u8,
        type_: usize,
        format: u8,
        data: Vec<u8>,
    },
    Refuse {
        target: usize,
    },
    NewValue {
        prop: u8,
        data: Vec<u8>,
    },
    Request {
        requestor: u8,
        selection: usize,
        target: usize,
        prop: usize,
        time: u16,
    },
    Deleted {
        window: u8,
        prop: usize,
    },
    Destroy {
        window: u8,
    },
    Clear,
    Serve,
    Restore,
    Advance {
        ms: u32,
    },
}

fn step() -> impl Strategy<Value = Step> {
    let n = 0..NAMES.len();
    prop_oneof![
        (n.clone(), proptest::option::of(2u8..6), 0u8..3, any::<u16>())
            .prop_map(|(name, owner, change, time)| Step::Owner { name, owner, change, time }),
        (n.clone(), 0u8..12, n.clone(), 0u8..4, proptest::collection::vec(any::<u8>(), 0..64))
            .prop_map(|(target, prop, type_, format, data)| Step::Reply {
                target,
                prop,
                type_,
                format,
                data
            }),
        n.clone().prop_map(|target| Step::Refuse { target }),
        (0u8..12, proptest::collection::vec(any::<u8>(), 0..16))
            .prop_map(|(prop, data)| Step::NewValue { prop, data }),
        (2u8..6, n.clone(), n.clone(), n.clone(), any::<u16>()).prop_map(
            |(requestor, selection, target, prop, time)| Step::Request {
                requestor,
                selection,
                target,
                prop,
                time
            }
        ),
        (2u8..6, n.clone()).prop_map(|(window, prop)| Step::Deleted { window, prop }),
        (2u8..6).prop_map(|window| Step::Destroy { window }),
        Just(Step::Clear),
        Just(Step::Serve),
        Just(Step::Restore),
        (0u32..70_000).prop_map(|ms| Step::Advance { ms }),
    ]
}

fn format(f: u8) -> Format {
    match f {
        0 => Format::Bits8,
        1 => Format::Bits16,
        _ => Format::Bits32,
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    #[test]
    fn no_sequence_wedges_the_engine(steps in proptest::collection::vec(step(), 0..120)) {
        let mut x = FakeX::new();
        let (mut e, _) = engine(&mut x);
        let atoms: Vec<Atom> = NAMES.iter().map(|n| x.a(n)).collect();
        let pick = |i: usize| atoms.get(i).copied().unwrap_or(Atom::NONE);
        let mut now = Millis(0);
        for s in steps {
            let ev = match s {
                // CLIPBOARD claims and deaths go through the fake server's rule, as real ones do;
                // anything about other selections is delivered raw.
                Step::Owner { name, owner, change, time } if name % 2 == 0 => {
                    let events = match (change % 3, owner) {
                        (0, Some(o)) => x.other_claims(Window(u32::from(o)), u32::from(time)),
                        (0, None) => Vec::new(),
                        _ => x.owner_dies(),
                    };
                    for ev in events {
                        e.handle(&mut x, ev, now);
                    }
                    None
                }
                Step::Owner { name, owner, change, time } => Some(Event::OwnerChanged {
                    selection: pick(name % 2),
                    owner: owner.map(|o| Window(u32::from(o))),
                    change: [OwnerChange::Set, OwnerChange::WindowDestroyed, OwnerChange::ClientClosed][usize::from(change % 3)],
                    timestamp: Timestamp(u32::from(time)),
                    selection_timestamp: Timestamp(u32::from(time)),
                }),
                Step::Reply { target, prop, type_, format: f, data } => {
                    let unit = format(f);
                    let data: Vec<u8> = data.iter().copied().take(data.len() / 4 * 4).collect();
                    let property = x.a(&format!("JACKSTAY_P{prop}"));
                    x.put(OURS, property, NAMES[type_], unit, &data);
                    Some(Event::SelectionNotify { selection: pick(0), target: pick(target), property: Some(property), time: Timestamp(0) })
                }
                Step::Refuse { target } => Some(Event::SelectionNotify { selection: pick(0), target: pick(target), property: None, time: Timestamp(0) }),
                Step::NewValue { prop, data } => {
                    let property = x.a(&format!("JACKSTAY_P{prop}"));
                    x.put(OURS, property, "UTF8_STRING", Format::Bits8, &data);
                    Some(Event::PropertyNotify { window: OURS, atom: property, state: PropertyState::NewValue, time: Timestamp(0) })
                }
                Step::Request { requestor, selection, target, prop, time } => Some(Event::SelectionRequest {
                    requestor: Window(u32::from(requestor)),
                    selection: pick(selection % 2),
                    target: pick(target),
                    property: Some(pick(prop)),
                    time: Timestamp(u32::from(time)),
                }),
                Step::Deleted { window, prop } => Some(Event::PropertyNotify { window: Window(u32::from(window)), atom: pick(prop), state: PropertyState::Deleted, time: Timestamp(0) }),
                Step::Destroy { window } => Some(Event::DestroyNotify { window: Window(u32::from(window)) }),
                Step::Clear => { e.clear(&mut x, now); None }
                Step::Serve => { e.serve(&mut x, text_content(&"y".repeat(300_000)), now); None }
                Step::Restore => { e.restore(&mut x, text_content("saved"), now); None }
                Step::Advance { ms } => {
                    now = now.after(u64::from(ms));
                    x.time = x.time.saturating_add(ms);
                    None
                }
            };
            if let Some(ev) = ev {
                e.handle(&mut x, ev, now);
            }
            prop_assert!(e.properties_in_use() <= 2, "{} properties in use", e.properties_in_use());
            if e.owns_clipboard() {
                let owner = x.owners.get(&pick(0)).and_then(|o| o.0);
                prop_assert_eq!(owner, Some(OURS), "the engine believes it owns CLIPBOARD");
            }
            // Catch up on every deadline that has passed, as the adapter's loop does.
            let mut turns = 0;
            while let Some(d) = e.next_deadline().filter(|&d| d <= now) {
                e.tick(&mut x, now);
                turns += 1;
                prop_assert!(turns < 100, "deadline {d} did not move after ticking at {now}");
            }
            if let Some(d) = e.next_deadline() {
                e.tick(&mut x, d);
                prop_assert!(e.next_deadline().is_none_or(|n| n > d), "ticking at {d} left the deadline at {d}");
                now = now.max(d);
            }
        }
    }
}
