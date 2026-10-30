//! The input thread's X side on a real X server (Xvfb), driven by a second client through `XTest`
//! as a mouse and keyboard would (docs/input.md, "Testing").

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use jackstay_input::{Action, Layout};
use jackstay_proto::link::{LOCK_CAPS, LOCK_NUM, Monitor, Side};
use jackstay_x11_input::{Display, Event, Grab, Watchdog};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

#[track_caller]
fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e:?}"),
    }
}

struct Xvfb {
    child: Child,
    display: String,
    _ready: BufReader<std::process::ChildStdout>,
}

static NEXT: AtomicU32 = AtomicU32::new(0);

impl Xvfb {
    /// A server `width`×`height`. -noreset: it never resets when its last client leaves, which
    /// would release every held key and hide a stuck one (docs/input.md, "Testing").
    fn start(width: u32, height: u32) -> Xvfb {
        let _ = std::fs::create_dir("/tmp/.X11-unix");
        let base = 300 + (std::process::id() % 100) * 64;
        let number = base + NEXT.fetch_add(1, Ordering::Relaxed) % 64;
        let script = format!(
            "exec setpriv --pdeathsig KILL Xvfb :{number} -displayfd 3 -noreset -nolisten tcp -screen 0 {width}x{height}x24 3>&1 1>/dev/null 2>/dev/null"
        );
        let mut child = ok(Command::new("sh").args(["-c", &script]).stdout(Stdio::piped()).spawn());
        let mut ready = BufReader::new(child.stdout.take().unwrap_or_else(|| panic!("stdout")));
        let mut line = String::new();
        let started = ready.read_line(&mut line).is_ok() && line.trim() == number.to_string();
        let x = Xvfb { child, display: format!(":{number}"), _ready: ready };
        assert!(started, "Xvfb :{number}: {line:?}");
        x
    }

    /// xiphos's three monitors, on a 3440×4080 server.
    fn xiphos() -> Xvfb {
        let x = Xvfb::start(3440, 4080);
        for (name, geometry, output) in [
            ("A", "3440/800x1440/300+0+1200", "screen"),
            ("B", "1920/500x1200/300+939+0", "none"),
            ("C", "3440/800x1440/300+0+2640", "none"),
        ] {
            x.run("xrandr", &["--setmonitor", name, geometry, output]);
        }
        x
    }

    fn run(&self, program: &str, args: &[&str]) {
        let status = ok(Command::new(program).args(args).env("DISPLAY", &self.display).status());
        assert!(status.success(), "{program} {args:?}");
    }

    fn display(&self) -> Display {
        ok(Display::connect(Some(&self.display)))
    }

    /// Another client: the test's hands on the mouse and keyboard.
    fn hands(&self) -> Hands {
        let (conn, screen) = ok(x11rb::connect(Some(&self.display)));
        let root = conn.setup().roots[screen].root;
        // XFixes answers nothing before its version is asked.
        ok(ok(x11rb::protocol::xfixes::ConnectionExt::xfixes_query_version(&conn, 6, 0)).reply());
        Hands { conn, root }
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Hands {
    conn: RustConnection,
    root: xproto::Window,
}

impl Hands {
    /// Relative motion, as a mouse makes.
    fn nudge(&self, dx: i16, dy: i16) {
        ok(self.conn.xtest_fake_input(xproto::MOTION_NOTIFY_EVENT, 1, 0, self.root, dx, dy, 0));
        ok(self.conn.sync());
    }

    fn warp(&self, x: i16, y: i16) {
        ok(self.conn.warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, x, y));
        ok(self.conn.sync());
    }

    fn key(&self, keycode: u8, down: bool) {
        let kind = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
        ok(self.conn.xtest_fake_input(kind, keycode, 0, self.root, 0, 0, 0));
        ok(self.conn.sync());
    }

    fn button(&self, button: u8, down: bool) {
        let kind = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        ok(self.conn.xtest_fake_input(kind, button, 0, self.root, 0, 0, 0));
        ok(self.conn.sync());
    }

    fn pointer(&self) -> (i16, i16) {
        let p = ok(ok(self.conn.query_pointer(self.root)).reply());
        (p.root_x, p.root_y)
    }

    fn key_down(&self, keycode: u8) -> bool {
        let keys = ok(ok(self.conn.query_keymap()).reply()).keys;
        keys[usize::from(keycode / 8)] & (1 << (keycode % 8)) != 0
    }

    /// Whether this client can grab the keyboard: no one else holds it.
    fn can_grab_keyboard(&self) -> bool {
        let r = ok(ok(self.conn.grab_keyboard(
            false,
            self.root,
            x11rb::CURRENT_TIME,
            xproto::GrabMode::ASYNC,
            xproto::GrabMode::ASYNC,
        ))
        .reply());
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.conn.sync();
        r.status == xproto::GrabStatus::SUCCESS
    }

    fn can_grab_pointer(&self) -> bool {
        let r = ok(ok(self.conn.grab_pointer(
            false,
            self.root,
            xproto::EventMask::NO_EVENT,
            xproto::GrabMode::ASYNC,
            xproto::GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            x11rb::CURRENT_TIME,
        ))
        .reply());
        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = self.conn.sync();
        r.status == xproto::GrabStatus::SUCCESS
    }
}

/// The events `d` gets within `within`, collected until `done` says so.
fn events_until(d: &mut Display, within: Duration, done: impl Fn(&[Event]) -> bool) -> Vec<Event> {
    let until = Instant::now() + within;
    let mut all = Vec::new();
    while Instant::now() < until {
        all.extend(ok(d.events()));
        if done(&all) {
            return all;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    all
}

fn settle(d: &mut Display) {
    let _ = events_until(d, Duration::from_millis(100), |_| false);
}

#[test]
fn the_monitors_are_read_as_randr_has_them() {
    let x = Xvfb::xiphos();
    let mut monitors = ok(x.display().monitors());
    monitors.sort_by_key(|m| (m.y, m.x));
    assert_eq!(
        monitors,
        vec![
            Monitor { x: 939, y: 0, width: 1920, height: 1200 },
            Monitor { x: 0, y: 1200, width: 3440, height: 1440 },
            Monitor { x: 0, y: 2640, width: 3440, height: 1440 },
        ]
    );
}

#[test]
fn a_monitor_change_is_reported() {
    let x = Xvfb::start(1920, 1080);
    let mut d = x.display();
    settle(&mut d);
    x.run("xrandr", &["--setmonitor", "A", "960/250x1080/280+0+0", "screen"]);
    let events = events_until(&mut d, Duration::from_secs(2), |e| e.contains(&Event::Layout));
    assert!(events.contains(&Event::Layout), "{events:?}");
}

#[test]
fn the_keymap_is_read() {
    let x = Xvfb::start(640, 480);
    let map = ok(x.display().keyboard_map());
    let key = |code: u8| map.keys[usize::from(code - map.first)];
    // evdev: 38 is a/A, 50 is Shift_L, 66 Caps_Lock.
    assert_eq!((key(38).level1, key(38).level2, key(38).modifier), (0x61, 0x41, false));
    assert!(key(50).modifier);
    assert_eq!(key(66).level1, 0xffe5);
    assert_ne!(map.num_lock, 0, "Num Lock is on a modifier");
}

#[test]
fn barriers_report_a_push_against_each_side_with_a_neighbour() {
    let x = Xvfb::xiphos();
    let mut d = x.display();
    let layout = ok(Layout::new(&ok(d.monitors())));
    ok(d.set_barriers(&[layout.edge(Side::Left), layout.edge(Side::Right)]));
    ok(d.sync());
    let hands = x.hands();
    hands.warp(3, 2000);
    settle(&mut d);
    hands.nudge(-10, 0);
    let hits = events_until(&mut d, Duration::from_secs(2), |e| {
        e.iter().any(|e| matches!(e, Event::BarrierHit { .. }))
    });
    assert!(
        hits.iter().any(|e| matches!(e, Event::BarrierHit { side: Side::Left, x: 0, y: 2000, dx, .. } if *dx < 0)),
        "{hits:?}"
    );
    hands.warp(3436, 3000);
    settle(&mut d);
    hands.nudge(10, 0);
    let hits = events_until(&mut d, Duration::from_secs(2), |e| {
        e.iter().any(|e| matches!(e, Event::BarrierHit { .. }))
    });
    assert!(
        hits.iter().any(|e| matches!(e, Event::BarrierHit { side: Side::Right, x: 3439, y: 3000, dx, .. } if *dx > 0)),
        "{hits:?}"
    );
    // No barrier on HDMI-2's left side at x 939: not xiphos's left side.
    hands.warp(945, 500);
    settle(&mut d);
    hands.nudge(-20, 0);
    let hits = events_until(&mut d, Duration::from_millis(300), |_| false);
    assert!(!hits.iter().any(|e| matches!(e, Event::BarrierHit { .. })), "{hits:?}");
}

#[test]
fn a_grab_takes_both_devices_hides_the_cursor_and_forwards_input() {
    let x = Xvfb::start(1920, 1080);
    let mut d = x.display();
    let hands = x.hands();
    hands.warp(960, 540);
    assert_eq!(ok(d.grab()), Grab::Grabbed);
    assert!(!hands.can_grab_keyboard() && !hands.can_grab_pointer());
    let image =
        ok(ok(x11rb::protocol::xfixes::ConnectionExt::xfixes_get_cursor_image(&hands.conn))
            .reply());
    assert!(image.cursor_image.iter().all(|p| p >> 24 == 0), "the cursor is invisible");
    settle(&mut d);
    hands.nudge(10, -3);
    hands.button(1, true);
    hands.button(1, false);
    hands.key(38, true);
    hands.key(38, false);
    let want = [
        Event::Motion { dx: 10 * 256, dy: -3 * 256, xtest: true },
        Event::Button { button: 1, down: true },
        Event::Button { button: 1, down: false },
        Event::Key { keycode: 38, down: true },
        Event::Key { keycode: 38, down: false },
    ];
    let got = events_until(&mut d, Duration::from_secs(2), |e| e.len() >= want.len());
    assert_eq!(got, want);
    ok(d.ungrab());
    ok(d.sync());
    assert!(hands.can_grab_keyboard() && hands.can_grab_pointer());
}

#[test]
fn a_held_key_is_forwarded_once_however_long_it_repeats() {
    let x = Xvfb::start(640, 480);
    x.run("xset", &["r", "rate", "200", "30"]);
    // First: the server does repeat a key held through XTest here, as another client sees it.
    let hands = x.hands();
    ok(ok(hands.conn.grab_keyboard(
        false,
        hands.root,
        x11rb::CURRENT_TIME,
        xproto::GrabMode::ASYNC,
        xproto::GrabMode::ASYNC,
    ))
    .reply());
    hands.key(38, true);
    std::thread::sleep(Duration::from_millis(600));
    hands.key(38, false);
    let mut presses = 0;
    while let Some(e) = ok(hands.conn.poll_for_event()) {
        presses += usize::from(matches!(e, x11rb::protocol::Event::KeyPress(_)));
    }
    assert!(presses >= 3, "the key repeated: {presses} presses");
    ok(hands.conn.ungrab_keyboard(x11rb::CURRENT_TIME));
    ok(hands.conn.sync());
    // Then: grabbing, the same hold is forwarded once.
    let mut d = x.display();
    assert_eq!(ok(d.grab()), Grab::Grabbed);
    settle(&mut d);
    hands.key(38, true);
    std::thread::sleep(Duration::from_millis(600));
    hands.key(38, false);
    let got = events_until(&mut d, Duration::from_millis(300), |_| false);
    assert_eq!(
        got,
        vec![Event::Key { keycode: 38, down: true }, Event::Key { keycode: 38, down: false }]
    );
}

#[test]
fn no_grab_while_another_client_holds_the_keyboard_and_nothing_is_left_held() {
    let x = Xvfb::start(640, 480);
    let mut d = x.display();
    let hands = x.hands();
    ok(ok(hands.conn.grab_keyboard(
        false,
        hands.root,
        x11rb::CURRENT_TIME,
        xproto::GrabMode::ASYNC,
        xproto::GrabMode::ASYNC,
    ))
    .reply());
    assert_eq!(ok(d.grab()), Grab::Refused);
    ok(d.sync());
    assert!(!d.grabbed());
    // The pointer grab that succeeded was released.
    assert!(hands.can_grab_pointer());
}

#[test]
fn no_grab_with_a_button_held() {
    let x = Xvfb::start(640, 480);
    let mut d = x.display();
    let hands = x.hands();
    hands.button(1, true);
    assert_eq!(ok(d.grab()), Grab::ButtonHeld);
    ok(d.sync());
    assert!(hands.can_grab_keyboard() && hands.can_grab_pointer());
    hands.button(1, false);
    assert_eq!(ok(d.grab()), Grab::Grabbed);
}

#[test]
fn injected_motion_keys_and_buttons_arrive() {
    let x = Xvfb::xiphos();
    let d = x.display();
    let map = ok(d.keyboard_map());
    let hands = x.hands();
    ok(d.inject(
        &[Action::MoveTo { x: 1000, y: 100 }, Action::Key { keycode: 38, down: true }],
        &map,
    ));
    ok(d.sync());
    assert_eq!(hands.pointer(), (1000, 100));
    assert!(hands.key_down(38));
    ok(d.inject(&[Action::Key { keycode: 38, down: false }], &map));
    ok(d.sync());
    assert!(!hands.key_down(38));
}

#[test]
fn locks_are_set_and_read_back() {
    let x = Xvfb::start(640, 480);
    let d = x.display();
    let map = ok(d.keyboard_map());
    ok(d.inject(&[Action::Locks { caps: true, num: true }], &map));
    ok(d.sync());
    assert_eq!(ok(d.held_modifiers(&map)).1, LOCK_CAPS | LOCK_NUM);
    ok(d.inject(&[Action::Locks { caps: false, num: true }], &map));
    ok(d.sync());
    assert_eq!(ok(d.held_modifiers(&map)).1, LOCK_NUM);
}

#[test]
fn held_modifiers_are_only_modifier_keys() {
    let x = Xvfb::start(640, 480);
    let d = x.display();
    let map = ok(d.keyboard_map());
    let hands = x.hands();
    hands.key(50, true);
    hands.key(38, true);
    let (held, _) = ok(d.held_modifiers(&map));
    assert_eq!(jackstay_input::Set::from_bitmap(&held).iter().collect::<Vec<_>>(), vec![50]);
    hands.key(38, false);
    hands.key(50, false);
}

#[test]
fn keys_left_held_by_a_dead_injector_are_released_at_start() {
    let x = Xvfb::start(640, 480);
    let hands = x.hands();
    {
        let d = x.display();
        let map = ok(d.keyboard_map());
        ok(d.inject(
            &[Action::Key { keycode: 38, down: true }, Action::Button { button: 1, down: true }],
            &map,
        ));
        ok(d.sync());
        // Down before the connection dies: a request still unread when a client hangs up can
        // be dropped with it, and then the key never went down at all.
        assert!(hands.key_down(38), "the injected key went down");
        // Dropped without releasing: as a crash would.
    }
    std::thread::sleep(Duration::from_millis(100));
    assert!(hands.key_down(38), "the X server keeps a dead client's XTest key down");
    let d = x.display();
    assert_eq!(ok(d.clear_xtest()), 2);
    assert!(!hands.key_down(38));
    assert_eq!(ok(d.clear_xtest()), 0);
}

#[test]
fn the_watchdog_releases_a_grab_its_thread_stopped_holding() {
    let x = Xvfb::start(640, 480);
    let mut d = x.display();
    let hands = x.hands();
    let dog = ok(Watchdog::start(ok(d.socket()), Duration::from_millis(300)));
    assert_eq!(ok(d.grab()), Grab::Grabbed);
    dog.beat(true);
    // Beating while grabbing: nothing happens.
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(100));
        dog.beat(true);
    }
    assert!(!dog.fired() && !hands.can_grab_keyboard());
    // The input thread stops.
    let stopped = Instant::now();
    while !hands.can_grab_keyboard() {
        assert!(stopped.elapsed() < Duration::from_secs(2), "the grab was never released");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(dog.fired());
    // 300 ms and a 100 ms tick, with room for a loaded machine.
    assert!(stopped.elapsed() < Duration::from_millis(1500), "{:?}", stopped.elapsed());
    assert!(hands.can_grab_pointer());
}

#[test]
fn the_watchdog_leaves_an_idle_thread_alone() {
    let x = Xvfb::start(640, 480);
    let d = x.display();
    let dog = ok(Watchdog::start(ok(d.socket()), Duration::from_millis(200)));
    dog.beat(false);
    std::thread::sleep(Duration::from_millis(600));
    assert!(!dog.fired());
    assert!(d.monitors().is_ok(), "the connection still works");
}

#[test]
fn a_keymap_change_is_reported() {
    let x = Xvfb::start(640, 480);
    let mut d = x.display();
    settle(&mut d);
    x.run("xmodmap", &["-e", "keycode 38 = b B"]);
    let events = events_until(&mut d, Duration::from_secs(2), |e| e.contains(&Event::Keymap));
    assert!(events.contains(&Event::Keymap), "xmodmap: {events:?}");
    assert_eq!(ok(d.keyboard_map()).keys[38 - 8].level1, 0x62);
    settle(&mut d);
    x.run("setxkbmap", &["-layout", "de"]);
    let events = events_until(&mut d, Duration::from_secs(2), |e| e.contains(&Event::Keymap));
    assert!(events.contains(&Event::Keymap), "setxkbmap: {events:?}");
}

#[test]
fn absolute_xtest_motion_is_forwarded_as_the_distance_moved() {
    let x = Xvfb::start(640, 480);
    let mut d = x.display();
    let hands = x.hands();
    ok(d.warp(320, 240));
    ok(d.sync());
    assert_eq!(ok(d.grab()), Grab::Grabbed);
    settle(&mut d);
    // As `xdotool mousemove` does: an absolute position, not a delta.
    ok(hands.conn.xtest_fake_input(xproto::MOTION_NOTIFY_EVENT, 0, 0, hands.root, 330, 245, 0));
    ok(hands.conn.sync());
    let got = events_until(&mut d, Duration::from_secs(1), |e| !e.is_empty());
    assert_eq!(got, vec![Event::Motion { dx: 10 * 256, dy: 5 * 256, xtest: true }]);
}
