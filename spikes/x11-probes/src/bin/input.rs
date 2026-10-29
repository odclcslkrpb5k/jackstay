//! M4 spike: the X behaviour input sharing's design rests on (docs/input.md), measured on Xvfb.
//!
//! 1. Edge barriers: do barriers on the screen's own edges (x = 0, x = W - 1, x = W) give
//!    `XI_BarrierHit` events when relative motion pushes against them?
//! 2. Grab: does `XIGrabDevice` on both master devices succeed from a barrier hit, with an
//!    invisible cursor, and does it fail with `AlreadyGrabbed` while another client holds the
//!    keyboard, or while a button is held in another client's window?
//! 3. Raw motion while grabbed: does `XI_RawMotion` arrive with deltas, even when the pointer is
//!    pinned at a screen edge, and does `WarpPointer` add raw events of its own?
//! 4. Repeats: is an XTest key held for 1 s auto-repeated, and how do repeats show in
//!    `XI_KeyPress` (flag) and `XI_RawKeyPress`?
//!
//! Prints one line per finding.

use std::error::Error;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

type R<T> = Result<T, Box<dyn Error>>;

const POINTER: u16 = 2; // the virtual core pointer
const KEYBOARD: u16 = 3; // the virtual core keyboard

fn main() -> R<()> {
    let (conn, n) = x11rb::connect(None)?;
    let screen = conn.setup().roots[n].clone();
    let (root, w, h) = (screen.root, screen.width_in_pixels, screen.height_in_pixels);
    conn.xfixes_query_version(6, 0)?.reply()?;
    conn.xinput_xi_query_version(2, 4)?.reply()?;
    println!("screen {w}x{h}");

    edge_barriers(&conn, root, w, h)?;
    grab_and_raw(&conn, root, w, h)?;
    grab_refused(&conn, root, w, h)?;
    repeats(&conn, root)?;
    Ok(())
}

fn select(conn: &RustConnection, root: u32, mask: xinput::XIEventMask) -> R<()> {
    conn.xinput_xi_select_events(root, &[xinput::EventMask {
        deviceid: u16::from(xinput::Device::ALL_MASTER),
        mask: vec![mask],
    }])?
    .check()?;
    Ok(())
}

fn events(conn: &RustConnection, wait: Duration) -> R<Vec<Event>> {
    conn.sync()?;
    std::thread::sleep(wait);
    let mut out = Vec::new();
    while let Some(e) = conn.poll_for_event()? {
        out.push(e);
    }
    Ok(out)
}

fn rel(conn: &RustConnection, root: u32, dx: i16, dy: i16) -> R<()> {
    conn.xtest_fake_input(xproto::MOTION_NOTIFY_EVENT, 1, 0, root, dx, dy, 0)?;
    Ok(())
}

fn pointer(conn: &RustConnection, root: u32) -> R<(i16, i16)> {
    let p = conn.query_pointer(root)?.reply()?;
    Ok((p.root_x, p.root_y))
}

fn fp1616(v: i32) -> f64 {
    f64::from(v) / 65536.0
}

fn fp3232(v: xinput::Fp3232) -> f64 {
    f64::from(v.integral) + f64::from(v.frac) / 4_294_967_296.0
}

/// 1: barriers on the screen's own edges.
fn edge_barriers(conn: &RustConnection, root: u32, w: u16, h: u16) -> R<()> {
    select(conn, root, xinput::XIEventMask::BARRIER_HIT | xinput::XIEventMask::BARRIER_LEAVE)?;
    for (label, x, start, push) in
        [("x=0", 0u16, 5i16, -4i16), ("x=W-1", w - 1, (w - 6) as i16, 4), ("x=W", w, (w - 6) as i16, 4)]
    {
        let b = conn.generate_id()?;
        conn.xfixes_create_pointer_barrier(b, root, x, 0, x, h, xfixes::BarrierDirections::from(0u32), &[])?
            .check()?;
        conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, start, 300)?.check()?;
        events(conn, Duration::from_millis(50))?;
        for _ in 0..6 {
            rel(conn, root, push, 0)?;
        }
        let evs = events(conn, Duration::from_millis(150))?;
        let hits: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                Event::XinputBarrierHit(b) => Some(format!("({:.0},{:.0} dx {:.1})", fp1616(b.root_x), fp1616(b.root_y), fp3232(b.dx))),
                _ => None,
            })
            .collect();
        let (px, _) = pointer(conn, root)?;
        println!("1 barrier {label}: pointer ends x={px}, {} hits {}", hits.len(), hits.join(" "));
        conn.xfixes_delete_pointer_barrier(b)?.check()?;
    }
    select(conn, root, xinput::XIEventMask::from(0u32))?;
    Ok(())
}

fn invisible_cursor(conn: &RustConnection, root: u32) -> R<u32> {
    let pix = conn.generate_id()?;
    conn.create_pixmap(1, pix, root, 1, 1)?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, pix, &xproto::CreateGCAux::new().foreground(0))?;
    conn.poly_fill_rectangle(pix, gc, &[xproto::Rectangle { x: 0, y: 0, width: 1, height: 1 }])?;
    let cursor = conn.generate_id()?;
    conn.create_cursor(cursor, pix, pix, 0, 0, 0, 0, 0, 0, 0, 0)?.check()?;
    Ok(cursor)
}

fn grab(conn: &RustConnection, root: u32, dev: u16, cursor: u32, mask: xinput::XIEventMask) -> R<xproto::GrabStatus> {
    Ok(conn
        .xinput_xi_grab_device(
            root,
            x11rb::CURRENT_TIME,
            cursor,
            xinput::DeviceId::from(dev),
            xproto::GrabMode::ASYNC,
            xproto::GrabMode::ASYNC,
            xinput::GrabOwner::NO_OWNER,
            &[u32::from(mask)],
        )?
        .reply()?
        .status)
}

/// 2 and 3: grab from a barrier hit with an invisible cursor; raw motion while grabbed.
fn grab_and_raw(conn: &RustConnection, root: u32, w: u16, h: u16) -> R<()> {
    let b = conn.generate_id()?;
    conn.xfixes_create_pointer_barrier(b, root, 0, 0, 0, h, xfixes::BarrierDirections::from(0u32), &[])?.check()?;
    select(conn, root, xinput::XIEventMask::BARRIER_HIT)?;
    conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 3, 300)?.check()?;
    rel(conn, root, -10, 0)?;
    let hit = events(conn, Duration::from_millis(100))?.iter().any(|e| matches!(e, Event::XinputBarrierHit(_)));
    let cursor = invisible_cursor(conn, root)?;
    let mask = xinput::XIEventMask::RAW_MOTION
        | xinput::XIEventMask::MOTION
        | xinput::XIEventMask::RAW_BUTTON_PRESS
        | xinput::XIEventMask::BUTTON_PRESS;
    let t = Instant::now();
    let ps = grab(conn, root, POINTER, cursor, mask)?;
    let ks = grab(
        conn,
        root,
        KEYBOARD,
        x11rb::NONE,
        xinput::XIEventMask::KEY_PRESS | xinput::XIEventMask::KEY_RELEASE | xinput::XIEventMask::RAW_KEY_PRESS,
    )?;
    println!("2 grab after barrier hit ({hit}): pointer {ps:?}, keyboard {ks:?}, both in {:?}", t.elapsed());
    let img = conn.xfixes_get_cursor_image()?.reply()?;
    let opaque = img.cursor_image.iter().filter(|p| *p >> 24 != 0).count();
    println!("2 cursor while grabbed: {}x{}, {opaque} opaque pixels", img.width, img.height);

    // Pinned at the edge: push further left, then down.
    events(conn, Duration::from_millis(20))?;
    for _ in 0..5 {
        rel(conn, root, -7, 0)?;
    }
    report_raw(conn, root, "pinned at x=0, 5 x (-7,0)")?;
    for _ in 0..5 {
        rel(conn, root, 0, 3)?;
    }
    report_raw(conn, root, "5 x (0,+3)")?;
    conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, (w / 2) as i16, (h / 2) as i16)?.check()?;
    report_raw(conn, root, "one WarpPointer to the centre")?;
    for _ in 0..3 {
        rel(conn, root, 10, 0)?;
    }
    report_raw(conn, root, "after the warp, 3 x (+10,0)")?;

    conn.xinput_xi_ungrab_device(x11rb::CURRENT_TIME, POINTER)?.check()?;
    conn.xinput_xi_ungrab_device(x11rb::CURRENT_TIME, KEYBOARD)?.check()?;
    let img = conn.xfixes_get_cursor_image()?.reply()?;
    let opaque = img.cursor_image.iter().filter(|p| *p >> 24 != 0).count();
    println!("2 cursor after ungrab: {}x{}, {opaque} opaque pixels", img.width, img.height);
    conn.xfixes_delete_pointer_barrier(b)?.check()?;
    select(conn, root, xinput::XIEventMask::from(0u32))?;
    Ok(())
}

fn report_raw(conn: &RustConnection, root: u32, label: &str) -> R<()> {
    let evs = events(conn, Duration::from_millis(80))?;
    let mut raws = Vec::new();
    let mut motions = 0;
    for e in &evs {
        match e {
            Event::XinputRawMotion(r) => {
                let processed: Vec<String> = r.axisvalues.iter().map(|v| format!("{:.1}", fp3232(*v))).collect();
                let raw: Vec<String> = r.axisvalues_raw.iter().map(|v| format!("{:.1}", fp3232(*v))).collect();
                raws.push(format!("[{}|{}]", processed.join(","), raw.join(",")));
            }
            Event::XinputMotion(_) => motions += 1,
            _ => {}
        }
    }
    let (x, y) = pointer(conn, root)?;
    println!("3 {label}: pointer at ({x},{y}); {} raw [processed|raw]: {}; {motions} motion events", raws.len(), raws.join(" "));
    Ok(())
}

/// 2: grabs refused while another client holds the keyboard, or a button in its window.
fn grab_refused(conn: &RustConnection, root: u32, w: u16, h: u16) -> R<()> {
    let (other, _) = x11rb::connect(None)?;
    let cursor = invisible_cursor(conn, root)?;
    let mask = xinput::XIEventMask::RAW_MOTION;
    other.grab_keyboard(false, root, x11rb::CURRENT_TIME, xproto::GrabMode::ASYNC, xproto::GrabMode::ASYNC)?.reply()?;
    let ks = grab(conn, root, KEYBOARD, x11rb::NONE, xinput::XIEventMask::KEY_PRESS)?;
    let ps = grab(conn, root, POINTER, cursor, mask)?;
    println!("2 while another client grabs the keyboard: keyboard {ks:?}, pointer {ps:?}");
    conn.xinput_xi_ungrab_device(x11rb::CURRENT_TIME, POINTER)?.check()?;
    other.ungrab_keyboard(x11rb::CURRENT_TIME)?.check()?;

    // A button held in another client's window: an implicit grab.
    let win = other.generate_id()?;
    other.create_window(0, win, root, 0, 0, w, h, 0, xproto::WindowClass::INPUT_OUTPUT, 0,
        &xproto::CreateWindowAux::new().event_mask(xproto::EventMask::BUTTON_PRESS | xproto::EventMask::BUTTON_RELEASE))?;
    other.map_window(win)?;
    other.sync()?;
    std::thread::sleep(Duration::from_millis(100));
    conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, (w / 2) as i16, (h / 2) as i16)?.check()?;
    conn.xtest_fake_input(xproto::BUTTON_PRESS_EVENT, 1, 0, root, 0, 0, 0)?;
    conn.sync()?;
    std::thread::sleep(Duration::from_millis(50));
    let ps = grab(conn, root, POINTER, cursor, mask)?;
    let ks = grab(conn, root, KEYBOARD, x11rb::NONE, xinput::XIEventMask::KEY_PRESS)?;
    println!("2 while button 1 is held in another client's window: pointer {ps:?}, keyboard {ks:?}");
    conn.xinput_xi_ungrab_device(x11rb::CURRENT_TIME, KEYBOARD)?.check()?;
    conn.xinput_xi_ungrab_device(x11rb::CURRENT_TIME, POINTER)?.check()?;
    conn.xtest_fake_input(xproto::BUTTON_RELEASE_EVENT, 1, 0, root, 0, 0, 0)?;
    other.destroy_window(win)?;
    other.sync()?;
    conn.sync()?;
    Ok(())
}

/// 4: auto-repeat of an XTest key held for 1 s, as a grabbing client sees it.
fn repeats(conn: &RustConnection, root: u32) -> R<()> {
    let ks = grab(
        conn,
        root,
        KEYBOARD,
        x11rb::NONE,
        xinput::XIEventMask::KEY_PRESS | xinput::XIEventMask::KEY_RELEASE | xinput::XIEventMask::RAW_KEY_PRESS
            | xinput::XIEventMask::RAW_KEY_RELEASE,
    )?;
    events(conn, Duration::from_millis(20))?;
    conn.xtest_fake_input(xproto::KEY_PRESS_EVENT, 38, 0, root, 0, 0, 0)?;
    let evs1 = events(conn, Duration::from_millis(1000))?;
    conn.xtest_fake_input(xproto::KEY_RELEASE_EVENT, 38, 0, root, 0, 0, 0)?;
    let evs2 = events(conn, Duration::from_millis(50))?;
    let (mut press, mut repeat, mut release, mut raw_press, mut raw_release) = (0, 0, 0, 0, 0);
    for e in evs1.iter().chain(&evs2) {
        match e {
            Event::XinputKeyPress(k) if k.flags.contains(xinput::KeyEventFlags::KEY_REPEAT) => repeat += 1,
            Event::XinputKeyPress(_) => press += 1,
            Event::XinputKeyRelease(_) => release += 1,
            Event::XinputRawKeyPress(_) => raw_press += 1,
            Event::XinputRawKeyRelease(_) => raw_release += 1,
            _ => {}
        }
    }
    println!("4 XTest key held 1 s, keyboard grab {ks:?}: KeyPress {press}, KeyPress+repeat flag {repeat}, KeyRelease {release}, RawKeyPress {raw_press}, RawKeyRelease {raw_release}");
    conn.xinput_xi_ungrab_device(x11rb::CURRENT_TIME, KEYBOARD)?.check()?;
    Ok(())
}
