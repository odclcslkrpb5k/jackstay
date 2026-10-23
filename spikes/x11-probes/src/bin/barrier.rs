//! Do XFixes pointer barriers act on XTest motion, and are XI 2.3 barrier events delivered?
//!
//! Places a vertical barrier at x = 500 that blocks both directions, warps the pointer to
//! (490, 300), then pushes right with XTest: relative motion first, then absolute motion. After
//! each, it prints where the pointer ended up and which barrier events arrived.

use std::error::Error;

use x11rb::connection::Connection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;

const BARRIER_X: u16 = 500;

fn main() -> Result<(), Box<dyn Error>> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let height = screen.height_in_pixels;

    let xf = conn.xfixes_query_version(6, 0)?.reply()?;
    let xi = conn.xinput_xi_query_version(2, 4)?.reply()?;
    println!("xfixes {}.{}, xinput {}.{}", xf.major_version, xf.minor_version,
        xi.major_version, xi.minor_version);

    conn.xinput_xi_select_events(root, &[xinput::EventMask {
        deviceid: u16::from(xinput::Device::ALL_MASTER),
        mask: vec![xinput::XIEventMask::BARRIER_HIT | xinput::XIEventMask::BARRIER_LEAVE],
    }])?.check()?;

    let barrier = conn.generate_id()?;
    conn.xfixes_create_pointer_barrier(barrier, root, BARRIER_X, 0, BARRIER_X, height,
        xfixes::BarrierDirections::from(0u32), &[])?.check()?;
    println!("barrier at x={BARRIER_X}, blocks both directions");

    for (label, relative) in [("relative", true), ("absolute", false)] {
        conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 490, 300)?.check()?;
        drain(&conn)?;
        for step in 1..=10i16 {
            // Relative motion takes deltas in both coordinates; absolute takes a position.
            let (detail, x, y) =
                if relative { (1u8, 5i16, 0i16) } else { (0u8, 490 + 5 * step, 300i16) };
            conn.xtest_fake_input(xproto::MOTION_NOTIFY_EVENT, detail, x11rb::CURRENT_TIME,
                root, x, y, 0)?;
        }
        conn.sync()?;
        std::thread::sleep(std::time::Duration::from_millis(200));
        let p = conn.query_pointer(root)?.reply()?;
        let (hits, leaves) = drain(&conn)?;
        println!("{label}: pushed from x=490 by +50, pointer now x={} y={}, \
            blocked={}, barrier_hit events={hits}, barrier_leave events={leaves}",
            p.root_x, p.root_y, p.root_x < i16::try_from(BARRIER_X)?);
    }
    Ok(())
}

fn drain(conn: &impl Connection) -> Result<(u32, u32), Box<dyn Error>> {
    conn.sync()?;
    let (mut hits, mut leaves) = (0, 0);
    while let Some(ev) = conn.poll_for_event()? {
        match ev {
            Event::XinputBarrierHit(_) => hits += 1,
            Event::XinputBarrierLeave(_) => leaves += 1,
            _ => {}
        }
    }
    Ok((hits, leaves))
}
