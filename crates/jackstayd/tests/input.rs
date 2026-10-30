//! Input sharing between two daemons through the relay (docs/input.md, "Testing"): A is the
//! primary on athena's layout, B a secondary on xiphos's, each naming the other on both sides as
//! the owner has them. A is driven as a mouse and keyboard drive it (relative `XTest` motion, keys
//! and buttons); what B's X server shows is read directly.

mod support;

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{PoisonError, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use jackstay_proto::control::{Error, InputState, InputStatus, Request, Response};
use support::link::{Faults, Machine, Rig};
use support::{Xvfb, ask, ok};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// The name each daemon gives itself, which is the other's peer name: both run here.
fn hostname() -> String {
    ok(std::fs::read_to_string("/proc/sys/kernel/hostname")).trim().to_owned()
}

/// An Xvfb whose `RandR` monitors are `monitors` (name, geometry), the first on the screen's
/// output.
fn screen(size: &str, monitors: &[(&str, &str)]) -> Xvfb {
    let x = Xvfb::with_screen(size);
    for (i, (name, geometry)) in monitors.iter().enumerate() {
        let output = if i == 0 { "screen" } else { "none" };
        let status =
            ok(x.command("xrandr").args(["--setmonitor", name, geometry, output]).status());
        assert!(status.success(), "xrandr --setmonitor {name}");
    }
    x
}

/// athena: four 2560×1440 monitors in a 2×2 grid.
fn athena() -> Xvfb {
    screen(
        "5120x2880x24",
        &[
            ("A", "2560/600x1440/340+0+0"),
            ("B", "2560/600x1440/340+2560+0"),
            ("C", "2560/600x1440/340+0+1440"),
            ("D", "2560/600x1440/340+2560+1440"),
        ],
    )
}

/// xiphos: a 1920×1200 monitor above two 3440×1440, with dead zones either side of it.
fn xiphos() -> Xvfb {
    screen(
        "3440x4080x24",
        &[
            ("A", "3440/800x1440/300+0+1200"),
            ("B", "1920/500x1200/300+939+0"),
            ("C", "3440/800x1440/300+0+2640"),
        ],
    )
}

/// One machine's X server, as a mouse and keyboard and as an onlooker.
struct Hands {
    conn: RustConnection,
    root: xproto::Window,
}

impl Hands {
    fn on(x: &Xvfb) -> Hands {
        let (conn, screen) = ok(x11rb::connect(Some(&x.display)));
        let root = conn.setup().roots[screen].root;
        Hands { conn, root }
    }

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

    fn pointer(&self) -> (i32, i32) {
        let p = ok(ok(self.conn.query_pointer(self.root)).reply());
        (i32::from(p.root_x), i32::from(p.root_y))
    }

    fn key_down(&self, keycode: u8) -> bool {
        let keys = ok(ok(self.conn.query_keymap()).reply()).keys;
        keys[usize::from(keycode / 8)] & (1 << (keycode % 8)) != 0
    }
}

fn input(m: &Machine) -> InputStatus {
    match ask(&m.home, Request::Input) {
        Response::Input(i) => i,
        other => panic!("{other:?}"),
    }
}

/// Waits up to `within` for `m`'s input to be in `state`.
#[track_caller]
fn input_in(m: &Machine, state: InputState, within: Duration) {
    let until = Instant::now() + within;
    loop {
        let i = input(m);
        if i.state == state {
            return;
        }
        assert!(Instant::now() < until, "not {state:?} within {within:?}: {i:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits up to `within` for `check`.
#[track_caller]
fn soon(what: &str, within: Duration, mut check: impl FnMut() -> bool) {
    let until = Instant::now() + within;
    while !check() {
        assert!(Instant::now() < until, "{what}: not within {within:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

const SOON: Duration = Duration::from_secs(5);

/// A rival is looked for among every process here: the test that runs one runs alone.
static RIVAL: RwLock<()> = RwLock::new(());

fn no_rival() -> RwLockReadGuard<'static, ()> {
    RIVAL.read().unwrap_or_else(PoisonError::into_inner)
}

/// The owner's two machines, linked, input running on both.
fn owners() -> Rig {
    let name = hostname();
    let sides = format!("left = \"{name}\"\nright = \"{name}\"\n");
    let a = Machine::with(athena(), &format!("[input]\nprimary = true\n{sides}"));
    let b = Machine::with(xiphos(), &format!("[input]\n{sides}"));
    let rig = Rig::of(a, b).link();
    input_in(&rig.a, InputState::Here, SOON);
    input_in(&rig.b, InputState::Secondary, SOON);
    rig
}

/// Moves B's pointer along its row to `x` by nudging A, a step at a time, each waited for (and
/// made again if X dropped it).
fn b_to_x(a: &Hands, b: &Hands, x: i32) {
    for _ in 0..2000 {
        let now = b.pointer().0;
        if now == x {
            return;
        }
        a.nudge(i16::try_from((x - now).clamp(-20, 20)).unwrap_or(0), 0);
        let until = Instant::now() + Duration::from_millis(200);
        while b.pointer().0 == now && Instant::now() < until {
            std::hint::spin_loop();
        }
    }
    panic!("B's pointer never reached x {x}");
}

/// Pushes A's pointer out through its left side at `y` until it is on B.
fn cross_left(rig: &Rig, a: &Hands, y: i16) {
    a.warp(5, y);
    soon("crossed", SOON, || {
        a.nudge(-10, 0);
        input(&rig.a).state == InputState::On
    });
}

#[test]
fn the_pointer_crosses_out_and_back_on_the_owners_layouts() {
    let _turn = no_rival();
    let rig = owners();
    let (a, b) = (Hands::on(&rig.a.x), Hands::on(&rig.b.x));
    cross_left(&rig, &a, 2000);
    assert_eq!(input(&rig.b).state, InputState::Driven);
    // Into xiphos's right side, one to one below its dead zones: 2000 is 3200 there.
    soon("B's pointer at the crossing", SOON, || {
        let (x, y) = b.pointer();
        y == 3200 && (3000..=3439).contains(&x)
    });
    // Back out through xiphos's right side, into athena's left at the same height, a pixel in.
    b_to_x(&a, &b, 3439);
    // One push across, made again only while the pointer is still on B (X may drop a nudge).
    let until = Instant::now() + SOON;
    while input(&rig.a).state != InputState::Here {
        assert!(
            Instant::now() < until,
            "not back: B's pointer at {:?}, B {:?}",
            b.pointer(),
            input(&rig.b)
        );
        a.nudge(5, 0);
        std::thread::sleep(Duration::from_millis(300));
    }
    assert_eq!(a.pointer(), (1, 2000));
    assert_eq!(input(&rig.b).state, InputState::Secondary);
}

#[test]
fn keys_typed_on_a_reach_b_and_a_black_hole_releases_them() {
    let _turn = no_rival();
    let rig = owners();
    let (a, b) = (Hands::on(&rig.a.x), Hands::on(&rig.b.x));
    cross_left(&rig, &a, 1000);
    // a, on both machines' keycode 38.
    a.key(38, true);
    soon("a down on B", SOON, || b.key_down(38));
    a.key(38, false);
    soon("a up on B", SOON, || !b.key_down(38));
    a.key(38, true);
    soon("a down on B again", SOON, || b.key_down(38));
    // Silence: B lets go of the key within its 300 ms, and A takes its pointer back.
    rig.relay.set(Faults { blackhole: true, ..Faults::default() });
    let silent = Instant::now();
    soon("a released on B", Duration::from_secs(2), || !b.key_down(38));
    let took = silent.elapsed();
    assert!(took < Duration::from_millis(1000), "released after {took:?}");
    soon("A home", Duration::from_secs(2), || input(&rig.a).state == InputState::Here);
    a.key(38, false);
    // Healed: what was late is dropped, and nothing is pressed again.
    rig.relay.set(Faults::default());
    std::thread::sleep(Duration::from_secs(2));
    assert!(!b.key_down(38));
    assert_eq!(input(&rig.b).state, InputState::Secondary);
}

#[test]
fn no_crossing_with_a_button_held() {
    let _turn = no_rival();
    let rig = owners();
    let a = Hands::on(&rig.a.x);
    a.warp(5, 500);
    a.button(1, true);
    for _ in 0..20 {
        a.nudge(-10, 0);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(input(&rig.a).state, InputState::Here);
    a.button(1, false);
    cross_left(&rig, &a, 500);
}

#[test]
fn the_return_key_and_switch_move_the_pointer() {
    let _turn = no_rival();
    let rig = owners();
    let (a, b) = (Hands::on(&rig.a.x), Hands::on(&rig.b.x));
    cross_left(&rig, &a, 700);
    // Scroll Lock (78): home, where it crossed.
    a.key(78, true);
    a.key(78, false);
    input_in(&rig.a, InputState::Here, SOON);
    let (x, y) = a.pointer();
    assert!(x <= 2 && (699..=701).contains(&y), "A's pointer at {x},{y}");
    // `jackstay switch`: to xiphos at the middle of the edge facing athena (its right), and back.
    let ask_a = |r: Request| ask(&rig.a.home, r);
    assert_eq!(
        ask_a(Request::Switch { peer: "nobody".into() }),
        Response::Error(Error::NoSuchPeer)
    );
    assert_eq!(ask_a(Request::Switch { peer: hostname() }), Response::Ok);
    input_in(&rig.a, InputState::On, SOON);
    soon("B's pointer at the middle", SOON, || b.pointer() == (3439, 2640));
    assert_eq!(ask_a(Request::SwitchHome), Response::Ok);
    input_in(&rig.a, InputState::Here, SOON);
    // Not on a secondary.
    assert_eq!(ask(&rig.b.home, Request::SwitchHome), Response::Error(Error::InputOff));
}

/// A program by another name: Synergy's, as far as the process list says.
fn named(name: &str) -> (PathBuf, Child) {
    let dir = std::env::temp_dir().join(format!("jackstay-rival-{}", std::process::id()));
    ok(std::fs::create_dir_all(&dir));
    let path = dir.join(name);
    ok(std::fs::copy("/bin/sleep", &path));
    let child = ok(Command::new(&path).arg("60").spawn());
    (dir, child)
}

#[test]
fn input_is_off_while_synergy_runs() {
    let _alone = RIVAL.write().unwrap_or_else(PoisonError::into_inner);
    let (dir, mut synergys) = named("synergys");
    let rig = {
        let name = hostname();
        let sides = format!("left = \"{name}\"\nright = \"{name}\"\n");
        let a = Machine::with(athena(), &format!("[input]\nprimary = true\n{sides}"));
        let b = Machine::with(xiphos(), &format!("[input]\n{sides}"));
        Rig::of(a, b).link()
    };
    let off = input(&rig.a);
    assert_eq!((off.state, off.detail.as_str()), (InputState::Off, "synergys is running"));
    let a = Hands::on(&rig.a.x);
    a.warp(5, 500);
    for _ in 0..20 {
        a.nudge(-10, 0);
    }
    assert_eq!(input(&rig.a).state, InputState::Off);
    // Stopping Synergy is enough: input starts within its 10 s check.
    let _ = synergys.kill();
    let _ = synergys.wait();
    input_in(&rig.a, InputState::Here, Duration::from_secs(12));
    cross_left(&rig, &a, 500);
    let _ = std::fs::remove_dir_all(dir);
}

/// The latency gate (docs/input.md, "Latency"): from a motion on A to B's X server showing it,
/// both seen from this process.
#[test]
fn motion_reaches_b_quickly() {
    let _turn = no_rival();
    let rig = owners();
    let (a, b) = (Hands::on(&rig.a.x), Hands::on(&rig.b.x));
    cross_left(&rig, &a, 2000);
    // Into the middle of xiphos, away from its edges.
    soon("B's pointer moved in", SOON, || {
        a.nudge(-50, 0);
        b.pointer().0 < 2000
    });
    // Each nudge is timed from when it is made to when B's pointer shows it, all one way, so
    // whatever arrives moves B on. `XTest` input is applied when Xvfb gets to it, and now and then
    // not at all: a nudge that shows nothing within 500 ms is counted, not timed.
    let mut took = Vec::new();
    let mut lost = 0;
    while took.len() < 200 {
        let before = b.pointer().1;
        let sent = Instant::now();
        a.nudge(0, 2);
        loop {
            if b.pointer().1 > before {
                took.push(sent.elapsed());
                break;
            }
            if sent.elapsed() > Duration::from_millis(500) {
                lost += 1;
                assert!(lost <= 10, "{lost} nudges showed nothing: A {:?}", input(&rig.a));
                break;
            }
        }
    }
    took.sort_unstable();
    let (median, p99, worst) = (took[100], took[197], took[199]);
    eprintln!("motion A to B: median {median:?}, p99 {p99:?}, worst {worst:?}, {lost} lost");
    // Loopback through the relay; loose enough for a loaded CI machine.
    assert!(median < Duration::from_millis(20), "median {median:?}");
    assert!(p99 < Duration::from_millis(100), "p99 {p99:?}");
}

#[test]
fn loss_from_b_alone_brings_the_pointer_home_and_leaves_b_ready() {
    let _turn = no_rival();
    let rig = owners();
    let (a, b) = (Hands::on(&rig.a.x), Hands::on(&rig.b.x));
    cross_left(&rig, &a, 1500);
    a.key(38, true);
    soon("a down on B", SOON, || b.key_down(38));
    // B still hears A, A hears nothing from B: A comes home and says so; B lets go, and has
    // nothing late to wait for.
    rig.relay.set(Faults { loss_to_a: 1.0, ..Faults::default() });
    soon("A home", Duration::from_secs(2), || input(&rig.a).state == InputState::Here);
    soon("a released on B", Duration::from_secs(2), || !b.key_down(38));
    a.key(38, false);
    rig.relay.set(Faults::default());
    // Heard again both ways: the next crossing types on B at once.
    soon("linked", SOON, || {
        std::thread::sleep(Duration::from_millis(100));
        rig.a.peer().0 == jackstay_proto::control::PeerState::Connected
    });
    cross_left(&rig, &a, 1500);
    a.key(38, true);
    soon("a down on B after", SOON, || b.key_down(38));
    a.key(38, false);
    soon("a up on B after", SOON, || !b.key_down(38));
}

#[test]
fn a_key_held_when_b_is_killed_is_released_when_b_restarts() {
    let _turn = no_rival();
    let mut rig = owners();
    let (a, b) = (Hands::on(&rig.a.x), Hands::on(&rig.b.x));
    cross_left(&rig, &a, 1500);
    a.key(38, true);
    soon("a down on B", SOON, || b.key_down(38));
    rig.b.signal("-KILL");
    if let Some(mut d) = rig.b.daemon.take() {
        let _ = d.child.wait();
    }
    // Held through XTest, the key outlives the daemon that pressed it...
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.key_down(38));
    // ...until the next start lets it go.
    rig.b.daemon = Some(support::Daemon::start(&rig.b.x, &rig.b.home));
    soon("a released on B", SOON, || !b.key_down(38));
    a.key(38, false);
}

#[test]
fn a_crossing_after_an_idle_spell_stays_crossed() {
    let _turn = no_rival();
    let rig = owners();
    // Idle: each side is heard about once a second, so the last frame may be most of a second old
    // when the pointer crosses.
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(1700));
        assert_eq!(ask(&rig.a.home, Request::Switch { peer: hostname() }), Response::Ok);
        input_in(&rig.a, InputState::On, SOON);
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(input(&rig.a).state, InputState::On, "bounced back");
        assert_eq!(ask(&rig.a.home, Request::SwitchHome), Response::Ok);
        input_in(&rig.a, InputState::Here, SOON);
    }
}

#[test]
fn loss_toward_b_alone_while_idle_leaves_b_ready() {
    let _turn = no_rival();
    let rig = owners();
    let (a, b) = (Hands::on(&rig.a.x), Hands::on(&rig.b.x));
    // B hears nothing for longer than its 3 s; A still hears B's pings, and notices nothing.
    rig.relay.set(Faults { loss_to_b: 1.0, ..Faults::default() });
    std::thread::sleep(Duration::from_secs(4));
    rig.relay.set(Faults::default());
    std::thread::sleep(Duration::from_secs(2));
    cross_left(&rig, &a, 1500);
    a.key(38, true);
    soon("a down on B", SOON, || b.key_down(38));
    a.key(38, false);
    soon("a up on B", SOON, || !b.key_down(38));
}
