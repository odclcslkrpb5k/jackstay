//! Do keys pressed through XTest stay down after the injecting client goes away?
//!
//! `keyhold press <keycode> <exit|abort|hang>` presses a key and then leaves without releasing
//! it: a normal exit, an abort, or by hanging until killed. `keyhold query <keycode>` reports
//! whether the server still has that key down.

use std::error::Error;

use x11rb::connection::Connection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let mode = args.get(1).map(String::as_str);
    let keycode: u8 = args.get(2).ok_or("keycode missing")?.parse()?;
    match mode {
        Some("press") => {
            conn.xtest_fake_input(xproto::KEY_PRESS_EVENT, keycode, x11rb::CURRENT_TIME, root,
                0, 0, 0)?;
            conn.sync()?;
            println!("pressed {keycode}, down={}", is_down(&conn, keycode)?);
            match args.get(3).map(String::as_str) {
                Some("exit") => std::process::exit(0),
                Some("abort") => std::process::abort(),
                Some("hang") => loop {
                    std::thread::sleep(std::time::Duration::from_secs(60));
                },
                _ => Err("how to leave: exit, abort or hang".into()),
            }
        }
        Some("query") => {
            println!("keycode {keycode} down={}", is_down(&conn, keycode)?);
            Ok(())
        }
        _ => Err("usage: keyhold press|query <keycode> [exit|abort|hang]".into()),
    }
}

fn is_down(conn: &impl Connection, keycode: u8) -> Result<bool, Box<dyn Error>> {
    let keys = conn.query_keymap()?.reply()?.keys;
    let byte = keys[usize::from(keycode / 8)];
    Ok(byte & (1 << (keycode % 8)) != 0)
}
