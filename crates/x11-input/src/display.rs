//! One X connection for input (docs/input.md): what the primary reads and grabs, and what the
//! secondary injects.

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use jackstay_input::{Action, Keymap, Set, Syms};
use jackstay_proto::link::{
    KeyEntry, LOCK_CAPS, LOCK_NUM, MAX_BUTTON, MAX_MONITORS, Monitor, Side,
};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// The master pointer and keyboard every X server has.
const MASTER_POINTER: u16 = 2;
const MASTER_KEYBOARD: u16 = 3;
/// The keysym of Num Lock, to find which modifier it is.
const NUM_LOCK: u32 = 0xff7f;
/// The device XKB requests address: the core keyboard.
const CORE_KBD: xkb::DeviceSpec = 0x100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Connect(String),
    /// The connection failed or the server refused a request.
    Connection(String),
    /// The server lacks something input sharing needs.
    Missing(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Connect(e) => write!(f, "cannot connect to X: {e}"),
            Error::Connection(e) => write!(f, "X connection: {e}"),
            Error::Missing(what) => write!(f, "the X server lacks {what}"),
        }
    }
}

impl std::error::Error for Error {}

fn x<E: std::fmt::Display>(e: E) -> Error {
    Error::Connection(e.to_string())
}

/// What happened on the X server, as the input thread needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The monitors may have changed: read them again.
    Layout,
    /// The keymap changed: read it again.
    Keymap,
    /// The pointer pushed against the barrier on `side` at (`x`, `y`), by (`dx`, `dy`) in 1/256 px.
    BarrierHit { side: Side, x: i32, y: i32, dx: i32, dy: i32 },
    /// Relative pointer motion, after acceleration, in 1/256 px; `xtest` if `XTest` made it.
    Motion { dx: i32, dy: i32, xtest: bool },
    /// A button while grabbing.
    Button { button: u8, down: bool },
    /// A key while grabbing; never the server's auto-repeats.
    Key { keycode: u8, down: bool },
}

/// How a grab attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grab {
    Grabbed,
    /// Another client holds a grab (a menu, a screen locker); nothing is held.
    Refused,
    /// A button is held: no crossing mid-drag; nothing is held.
    ButtonHeld,
}

/// This machine's keymap as input sharing needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyboardMap {
    pub first: u8,
    /// Each keycode's group 1, levels 1 and 2, from `first`, with whether it is a modifier.
    pub keys: Vec<KeyEntry>,
    /// The real modifier Num Lock sets, if any key has it.
    pub num_lock: u16,
}

impl KeyboardMap {
    #[must_use]
    pub fn keymap(&self) -> Keymap {
        let syms: Vec<Syms> =
            self.keys.iter().map(|k| Syms { level1: k.level1, level2: k.level2 }).collect();
        Keymap::new(self.first, &syms)
    }

    #[must_use]
    pub fn modifiers(&self) -> Set {
        let mut set = Set::default();
        for (code, k) in (self.first..=u8::MAX).zip(&self.keys) {
            if k.modifier {
                set.insert(code);
            }
        }
        set
    }
}

/// A slave pointer device's first two valuators, x and y, are relative.
#[derive(Debug, Clone, Copy)]
struct Device {
    relative: bool,
}

#[derive(Debug)]
pub struct Display {
    conn: RustConnection,
    root: xproto::Window,
    /// The invisible cursor the pointer grab shows.
    invisible: xproto::Cursor,
    barriers: HashMap<u32, Side>,
    devices: HashMap<u16, Device>,
    xtest_pointer: Option<u16>,
    xtest_keyboard: Option<u16>,
    grabbed: bool,
    /// Where the pointer is, in 1/256 px, as the last motion or warp left it: `XTest`'s motion is
    /// measured from it.
    last: Option<(i32, i32)>,
    /// The keymap as last read, to tell a real change from a device switch.
    map: Option<KeyboardMap>,
}

/// An `Fp3232` in 1/256 px.
fn sub(v: xinput::Fp3232) -> i32 {
    let whole = i64::from(v.integral).saturating_mul(256);
    i32::try_from(whole.saturating_add(i64::from(v.frac >> 24))).unwrap_or(0)
}

/// An `Fp1616` in whole pixels, rounded down.
fn pixel(v: xinput::Fp1616) -> i32 {
    v >> 16
}

/// Whether bit `n` is set in an XI2 mask.
fn bit(mask: &[u32], n: usize) -> bool {
    mask.get(n / 32).is_some_and(|w| w & (1 << (n % 32)) != 0)
}

impl Display {
    /// Connects to `display` (or `$DISPLAY`) and selects what input sharing listens to.
    ///
    /// # Errors
    /// No connection, or an extension missing: `XInput` 2.3 (barrier events), `XFixes` 5, `XTest`,
    /// `RandR` 1.5 (monitors), XKB.
    pub fn connect(display: Option<&str>) -> Result<Display, Error> {
        let (conn, screen) = x11rb::connect(display).map_err(|e| Error::Connect(e.to_string()))?;
        let root = conn.setup().roots.get(screen).ok_or(Error::Missing("a screen"))?.root;
        for (name, missing) in [
            (xinput::X11_EXTENSION_NAME, "XInput"),
            (xfixes::X11_EXTENSION_NAME, "XFixes"),
            (x11rb::protocol::xtest::X11_EXTENSION_NAME, "XTest"),
            (randr::X11_EXTENSION_NAME, "RandR"),
            (xkb::X11_EXTENSION_NAME, "XKB"),
        ] {
            if conn.extension_information(name).map_err(x)?.is_none() {
                return Err(Error::Missing(missing));
            }
        }
        let xi = conn.xinput_xi_query_version(2, 4).map_err(x)?.reply().map_err(x)?;
        if (xi.major_version, xi.minor_version) < (2, 3) {
            return Err(Error::Missing("XInput 2.3"));
        }
        let xf = conn.xfixes_query_version(6, 0).map_err(x)?.reply().map_err(x)?;
        if xf.major_version < 5 {
            return Err(Error::Missing("XFixes 5"));
        }
        let rr = conn.randr_query_version(1, 6).map_err(x)?.reply().map_err(x)?;
        if (rr.major_version, rr.minor_version) < (1, 5) {
            return Err(Error::Missing("RandR 1.5"));
        }
        let kb = conn.xkb_use_extension(1, 0).map_err(x)?.reply().map_err(x)?;
        if !kb.supported {
            return Err(Error::Missing("XKB 1.0"));
        }
        conn.xinput_xi_select_events(
            root,
            &[
                xinput::EventMask {
                    deviceid: u16::from(xinput::Device::ALL_MASTER),
                    mask: vec![xinput::XIEventMask::BARRIER_HIT | xinput::XIEventMask::RAW_MOTION],
                },
                // The server takes hierarchy changes only for all devices.
                xinput::EventMask {
                    deviceid: u16::from(xinput::Device::ALL),
                    mask: vec![xinput::XIEventMask::HIERARCHY],
                },
            ],
        )
        .map_err(x)?
        .check()
        .map_err(x)?;
        conn.randr_select_input(
            root,
            randr::NotifyMask::SCREEN_CHANGE
                | randr::NotifyMask::CRTC_CHANGE
                | randr::NotifyMask::OUTPUT_CHANGE,
        )
        .map_err(x)?;
        conn.change_window_attributes(
            root,
            &xproto::ChangeWindowAttributesAux::new()
                .event_mask(xproto::EventMask::STRUCTURE_NOTIFY),
        )
        .map_err(x)?;
        // An XKB client no longer gets the core MappingNotify for a changed keymap, nor anything
        // for a new keyboard (*measured*: xmodmap and setxkbmap went unreported): ask XKB.
        let keymap_parts = xkb::MapPart::KEY_SYMS | xkb::MapPart::MODIFIER_MAP;
        conn.xkb_select_events(
            CORE_KBD,
            xkb::EventType::from(0u16),
            xkb::EventType::NEW_KEYBOARD_NOTIFY | xkb::EventType::MAP_NOTIFY,
            keymap_parts,
            keymap_parts,
            &xkb::SelectEventsAux::new(),
        )
        .map_err(x)?
        .check()
        .map_err(x)?;
        let invisible = invisible_cursor(&conn, root)?;
        let mut d = Display {
            conn,
            root,
            invisible,
            barriers: HashMap::new(),
            devices: HashMap::new(),
            xtest_pointer: None,
            xtest_keyboard: None,
            grabbed: false,
            last: None,
            map: None,
        };
        d.read_devices()?;
        d.map = Some(d.keyboard_map()?);
        d.conn.flush().map_err(x)?;
        Ok(d)
    }

    /// The connection's socket, for the loop's `poll`. Every call here that waits for a reply
    /// reads whatever else is waiting on the socket into the connection's own queue, where
    /// `poll` cannot see it: the loop calls [`Display::events`] after any such call, and always
    /// right before polling.
    #[must_use]
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.conn.stream().as_fd()
    }

    /// A duplicate of the connection's socket, for the [`crate::Watchdog`].
    ///
    /// # Errors
    /// The descriptor could not be duplicated.
    pub fn socket(&self) -> std::io::Result<OwnedFd> {
        self.conn.stream().as_fd().try_clone_to_owned()
    }

    fn read_devices(&mut self) -> Result<(), Error> {
        let reply = self
            .conn
            .xinput_xi_query_device(xinput::DeviceId::from(xinput::Device::ALL))
            .map_err(x)?
            .reply()
            .map_err(x)?;
        self.devices.clear();
        self.xtest_pointer = None;
        self.xtest_keyboard = None;
        for info in reply.infos {
            let name = String::from_utf8_lossy(&info.name);
            if info.type_ == xinput::DeviceType::SLAVE_POINTER && name.contains("XTEST") {
                self.xtest_pointer = Some(info.deviceid);
            }
            if info.type_ == xinput::DeviceType::SLAVE_KEYBOARD && name.contains("XTEST") {
                self.xtest_keyboard = Some(info.deviceid);
            }
            let relative = info.classes.iter().any(|c| {
                matches!(&c.data, xinput::DeviceClassData::Valuator(v)
                    if v.number == 0 && v.mode == xinput::ValuatorMode::RELATIVE)
            });
            self.devices.insert(info.deviceid, Device { relative });
        }
        Ok(())
    }

    /// The monitors, at most [`MAX_MONITORS`]; any that cannot travel (at a negative position,
    /// empty, past 32767) are left out.
    ///
    /// # Errors
    /// The connection failed.
    pub fn monitors(&self) -> Result<Vec<Monitor>, Error> {
        let reply = self.conn.randr_get_monitors(self.root, true).map_err(x)?.reply().map_err(x)?;
        Ok(reply
            .monitors
            .iter()
            .filter_map(|m| {
                let monitor = Monitor {
                    x: u16::try_from(m.x).ok()?,
                    y: u16::try_from(m.y).ok()?,
                    width: m.width,
                    height: m.height,
                };
                monitor.check().ok().map(|()| monitor)
            })
            .take(MAX_MONITORS)
            .collect())
    }

    /// The keymap: each keycode's keysyms (group 1, levels 1 and 2), which are modifiers, and
    /// Num Lock's modifier.
    ///
    /// # Errors
    /// The connection failed.
    pub fn keyboard_map(&self) -> Result<KeyboardMap, Error> {
        let setup = self.conn.setup();
        let (first, last) = (setup.min_keycode.max(8), setup.max_keycode);
        let count = last.saturating_sub(first).saturating_add(1);
        let map = self.conn.get_keyboard_mapping(first, count).map_err(x)?.reply().map_err(x)?;
        let per = usize::from(map.keysyms_per_keycode);
        let mods = self.conn.get_modifier_mapping().map_err(x)?.reply().map_err(x)?;
        let mut modifier = Set::default();
        for k in mods.keycodes.iter().filter(|k| **k != 0) {
            modifier.insert(*k);
        }
        let syms = |code: u8, level: usize| -> u32 {
            let i =
                usize::from(code.saturating_sub(first)).saturating_mul(per).saturating_add(level);
            if level < per { map.keysyms.get(i).copied().unwrap_or(0) } else { 0 }
        };
        let keys: Vec<KeyEntry> = (first..=last)
            .map(|code| KeyEntry {
                level1: syms(code, 0),
                level2: syms(code, 1),
                modifier: modifier.contains(code),
            })
            .collect();
        // Which of the eight modifiers has a key whose first keysym is Num Lock.
        let per_mod = usize::from(mods.keycodes_per_modifier()).max(1);
        let num_lock = mods
            .keycodes
            .chunks(per_mod)
            .enumerate()
            .find(|(_, row)| row.iter().any(|k| *k != 0 && syms(*k, 0) == NUM_LOCK))
            .map_or(0, |(i, _)| 1u16 << i.min(7));
        Ok(KeyboardMap { first, keys, num_lock })
    }

    /// Puts a barrier on each segment of each of `edges`' sides, replacing any before.
    ///
    /// # Errors
    /// The connection failed.
    pub fn set_barriers(&mut self, edges: &[jackstay_input::Edge]) -> Result<(), Error> {
        for b in self.barriers.keys() {
            self.conn.xfixes_delete_pointer_barrier(*b).map_err(x)?;
        }
        self.barriers.clear();
        for edge in edges {
            // At the screen's own edge: the right and bottom barriers one past the last pixel,
            // where the pointer still reaches it (*measured*: at the last pixel it stops short).
            let line = match edge.side {
                Side::Left | Side::Top => edge.across,
                Side::Right | Side::Bottom => edge.across.saturating_add(1),
            };
            for &(start, end) in &edge.spans {
                let c = |v: i32| u16::try_from(v).unwrap_or(u16::MAX);
                let (x1, y1, x2, y2) = match edge.side {
                    Side::Left | Side::Right => (c(line), c(start), c(line), c(end)),
                    Side::Top | Side::Bottom => (c(start), c(line), c(end), c(line)),
                };
                let id = self.conn.generate_id().map_err(x)?;
                self.conn
                    .xfixes_create_pointer_barrier(
                        id,
                        self.root,
                        x1,
                        y1,
                        x2,
                        y2,
                        xfixes::BarrierDirections::from(0u32),
                        &[],
                    )
                    .map_err(x)?;
                self.barriers.insert(id, edge.side);
            }
        }
        self.conn.flush().map_err(x)
    }

    /// The pointer's position and whether any button 1–[`MAX_BUTTON`] is held.
    ///
    /// # Errors
    /// The connection failed.
    pub fn pointer(&self) -> Result<(i32, i32, bool), Error> {
        let p = self
            .conn
            .xinput_xi_query_pointer(self.root, MASTER_POINTER)
            .map_err(x)?
            .reply()
            .map_err(x)?;
        let held = (1..=usize::from(MAX_BUTTON)).any(|b| bit(&p.buttons, b));
        Ok((pixel(p.root_x), pixel(p.root_y), held))
    }

    /// Grabs the pointer (with an invisible cursor) and the keyboard, unless a button is held
    /// before or after. Never leaves one grabbed without the other (docs/input.md, "Primary").
    ///
    /// # Errors
    /// The connection failed.
    pub fn grab(&mut self) -> Result<Grab, Error> {
        if self.pointer()?.2 {
            return Ok(Grab::ButtonHeld);
        }
        // Raw motion selected on the root does not reach the grabbing client (*measured*): the
        // grab asks for it too.
        // Motion too: XTest's raw motion can be absolute (`xdotool mousemove`), so its distance
        // is measured from the pointer's positions instead (*measured*: an absolute move to
        // (700, 600) came as raw values 700 and 600).
        let pointer_mask = xinput::XIEventMask::BUTTON_PRESS
            | xinput::XIEventMask::BUTTON_RELEASE
            | xinput::XIEventMask::RAW_MOTION
            | xinput::XIEventMask::MOTION;
        let keyboard_mask = xinput::XIEventMask::KEY_PRESS | xinput::XIEventMask::KEY_RELEASE;
        let p = self.grab_device(MASTER_POINTER, self.invisible, pointer_mask)?;
        if p != xproto::GrabStatus::SUCCESS {
            return Ok(Grab::Refused);
        }
        // From here the pointer is held: every way out releases it, an error included.
        self.grabbed = true;
        let k = match self.grab_device(MASTER_KEYBOARD, x11rb::NONE, keyboard_mask) {
            Ok(k) => k,
            Err(e) => {
                let _ = self.ungrab();
                return Err(e);
            }
        };
        if k != xproto::GrabStatus::SUCCESS {
            self.ungrab()?;
            return Ok(Grab::Refused);
        }
        // A button that went down between the check and the grab.
        let (px, py, held) = match self.pointer() {
            Ok(p) => p,
            Err(e) => {
                let _ = self.ungrab();
                return Err(e);
            }
        };
        if held {
            self.ungrab()?;
            return Ok(Grab::ButtonHeld);
        }
        self.last = Some((px.saturating_mul(256), py.saturating_mul(256)));
        Ok(Grab::Grabbed)
    }

    fn grab_device(
        &self,
        device: u16,
        cursor: xproto::Cursor,
        mask: xinput::XIEventMask,
    ) -> Result<xproto::GrabStatus, Error> {
        Ok(self
            .conn
            .xinput_xi_grab_device(
                self.root,
                x11rb::CURRENT_TIME,
                cursor,
                xinput::DeviceId::from(device),
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
                xinput::GrabOwner::NO_OWNER,
                &[u32::from(mask)],
            )
            .map_err(x)?
            .reply()
            .map_err(x)?
            .status)
    }

    fn ungrab_devices(&self) -> Result<(), Error> {
        for device in [MASTER_KEYBOARD, MASTER_POINTER] {
            self.conn.xinput_xi_ungrab_device(x11rb::CURRENT_TIME, device).map_err(x)?;
        }
        self.conn.flush().map_err(x)
    }

    /// Releases both grabs.
    ///
    /// # Errors
    /// The connection failed.
    pub fn ungrab(&mut self) -> Result<(), Error> {
        self.grabbed = false;
        self.ungrab_devices()
    }

    #[must_use]
    pub fn grabbed(&self) -> bool {
        self.grabbed
    }

    /// Moves the pointer to (`x`, `y`): a warp, which makes no raw motion.
    ///
    /// # Errors
    /// The connection failed.
    pub fn warp(&mut self, x_: i32, y_: i32) -> Result<(), Error> {
        self.last = Some((x_.saturating_mul(256), y_.saturating_mul(256)));
        let c = |v: i32| i16::try_from(v).unwrap_or(i16::MAX);
        self.conn.warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, c(x_), c(y_)).map_err(x)?;
        self.conn.flush().map_err(x)
    }

    /// The modifier keys held now (a keycode bitmap, only keys in `modifiers`), and the locks on.
    ///
    /// # Errors
    /// The connection failed.
    pub fn held_modifiers(&self, map: &KeyboardMap) -> Result<([u8; 32], u8), Error> {
        let keys = self.conn.query_keymap().map_err(x)?.reply().map_err(x)?.keys;
        let modifiers = map.modifiers();
        let mut held = Set::default();
        for code in Set::from_bitmap(&keys).iter().filter(|k| modifiers.contains(*k)) {
            held.insert(code);
        }
        let state = self.conn.xkb_get_state(CORE_KBD).map_err(x)?.reply().map_err(x)?;
        let locked = u16::from(state.locked_mods);
        let mut locks = 0;
        if locked & u16::from(xproto::ModMask::LOCK) != 0 {
            locks |= LOCK_CAPS;
        }
        if map.num_lock != 0 && locked & map.num_lock != 0 {
            locks |= LOCK_NUM;
        }
        Ok((held.to_bitmap(), locks))
    }

    /// Does what the secondary's state machine says, and flushes at once.
    ///
    /// # Errors
    /// The connection failed.
    pub fn inject(&self, actions: &[Action], map: &KeyboardMap) -> Result<(), Error> {
        for a in actions {
            match *a {
                Action::MoveTo { x: px, y: py } => {
                    let c = |v: i32| i16::try_from(v).unwrap_or(i16::MAX);
                    self.fake(xproto::MOTION_NOTIFY_EVENT, 0, c(px), c(py))?;
                }
                Action::Button { button, down } => {
                    let kind = if down {
                        xproto::BUTTON_PRESS_EVENT
                    } else {
                        xproto::BUTTON_RELEASE_EVENT
                    };
                    self.fake(kind, button, 0, 0)?;
                }
                Action::Key { keycode, down } => {
                    let kind =
                        if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
                    self.fake(kind, keycode, 0, 0)?;
                }
                Action::Locks { caps, num } => self.set_locks(caps, num, map)?,
                Action::Leave { .. } => {}
            }
        }
        self.conn.flush().map_err(x)
    }

    fn fake(&self, kind: u8, detail: u8, px: i16, py: i16) -> Result<(), Error> {
        self.conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, px, py, 0)
            .map_err(x)?;
        Ok(())
    }

    fn set_locks(&self, caps: bool, num: bool, map: &KeyboardMap) -> Result<(), Error> {
        let lock = u16::from(xproto::ModMask::LOCK);
        let affect = lock | map.num_lock;
        let mut on = 0;
        if caps {
            on |= lock;
        }
        if num {
            on |= map.num_lock;
        }
        self.conn
            .xkb_latch_lock_state(
                CORE_KBD,
                xproto::ModMask::from(affect),
                xproto::ModMask::from(on),
                false,
                xkb::Group::M1,
                xproto::ModMask::from(0u16),
                false,
                0,
            )
            .map_err(x)?;
        Ok(())
    }

    /// Releases every key and button `XTest` holds: a daemon that died mid-keypress leaves them
    /// down (docs/input.md, "Secondary"). Returns how many it released.
    ///
    /// # Errors
    /// The connection failed.
    pub fn clear_xtest(&self) -> Result<usize, Error> {
        let mut released = 0usize;
        for (device, key) in [(self.xtest_keyboard, true), (self.xtest_pointer, false)] {
            let Some(id) = device.and_then(|d| u8::try_from(d).ok()) else { continue };
            let state = self.conn.xinput_query_device_state(id).map_err(x)?.reply().map_err(x)?;
            for class in state.classes {
                let (bits, kind, low) = match (&class.data, key) {
                    (xinput::InputStateData::Key(k), true) => {
                        (k.keys, xproto::KEY_RELEASE_EVENT, 8)
                    }
                    (xinput::InputStateData::Button(b), false) => {
                        (b.buttons, xproto::BUTTON_RELEASE_EVENT, 1)
                    }
                    _ => continue,
                };
                for code in Set::from_bitmap(&bits).iter().filter(|c| *c >= low) {
                    self.fake(kind, code, 0, 0)?;
                    released = released.saturating_add(1);
                }
            }
        }
        self.conn.flush().map_err(x)?;
        Ok(released)
    }

    /// Waits until the server has handled every request sent so far: another client's view of
    /// the server after this includes them.
    ///
    /// # Errors
    /// The connection failed.
    pub fn sync(&self) -> Result<(), Error> {
        self.conn.get_input_focus().map_err(x)?.reply().map_err(x)?;
        Ok(())
    }

    /// The events waiting, without blocking.
    ///
    /// # Errors
    /// The connection failed.
    pub fn events(&mut self) -> Result<Vec<Event>, Error> {
        let mut out = Vec::new();
        let mut keymap = false;
        while let Some(e) = self.conn.poll_for_event().map_err(x)? {
            match e {
                XEvent::XinputBarrierHit(b) => {
                    if let Some(side) = self.barriers.get(&b.barrier) {
                        out.push(Event::BarrierHit {
                            side: *side,
                            x: pixel(b.root_x),
                            y: pixel(b.root_y),
                            dx: sub(b.dx),
                            dy: sub(b.dy),
                        });
                    }
                }
                XEvent::XinputMotion(m) => {
                    // Fp1616 to 1/256 px.
                    let at = (m.root_x >> 8, m.root_y >> 8);
                    if Some(m.sourceid) == self.xtest_pointer
                        && let Some((lx, ly)) = self.last
                    {
                        let (dx, dy) = (at.0.saturating_sub(lx), at.1.saturating_sub(ly));
                        if dx != 0 || dy != 0 {
                            out.push(Event::Motion { dx, dy, xtest: true });
                        }
                    }
                    self.last = Some(at);
                }
                XEvent::XinputRawMotion(r) => {
                    let relative = self.devices.get(&r.sourceid).is_some_and(|d| d.relative);
                    // XTest's is measured from positions while grabbing (above); its raw values
                    // may be absolute. Outside a grab only its source matters.
                    let xtest = Some(r.sourceid) == self.xtest_pointer;
                    if !relative || (xtest && self.grabbed) {
                        continue;
                    }
                    // The values are packed: one per set bit of the mask, in order.
                    let mut values = r.axisvalues.iter();
                    let (mut dx, mut dy) = (0, 0);
                    for n in 0..r.valuator_mask.len().saturating_mul(32) {
                        if !bit(&r.valuator_mask, n) {
                            continue;
                        }
                        let v = values.next().copied().map_or(0, sub);
                        match n {
                            0 => dx = v,
                            1 => dy = v,
                            _ => {}
                        }
                    }
                    if dx != 0 || dy != 0 {
                        out.push(Event::Motion { dx, dy, xtest });
                    }
                }
                XEvent::XinputButtonPress(b) | XEvent::XinputButtonRelease(b) => {
                    let down = b.event_type == xinput::BUTTON_PRESS_EVENT;
                    if let Ok(button) = u8::try_from(b.detail)
                        && (1..=MAX_BUTTON).contains(&button)
                    {
                        out.push(Event::Button { button, down });
                    }
                }
                XEvent::XinputKeyPress(k) | XEvent::XinputKeyRelease(k) => {
                    let down = k.event_type == xinput::KEY_PRESS_EVENT;
                    let repeat = k.flags.contains(xinput::KeyEventFlags::KEY_REPEAT);
                    if let Ok(keycode) = u8::try_from(k.detail)
                        && !(down && repeat)
                    {
                        out.push(Event::Key { keycode, down });
                    }
                }
                XEvent::XinputHierarchy(_) => self.read_devices()?,
                XEvent::RandrScreenChangeNotify(_)
                | XEvent::RandrNotify(_)
                | XEvent::ConfigureNotify(_) => out.push(Event::Layout),
                // A new keyboard is also announced each time typing moves from one keyboard
                // device to another (XTest's included), with the same details as `setxkbmap`'s
                // (*measured*): so the map is read again, and reported only if it changed.
                XEvent::MappingNotify(_) => keymap = true,
                XEvent::XkbMapNotify(m) if u16::from(m.device_id) == MASTER_KEYBOARD => {
                    keymap = true;
                }
                XEvent::XkbNewKeyboardNotify(n) if u16::from(n.device_id) == MASTER_KEYBOARD => {
                    keymap = true;
                }
                _ => {}
            }
        }
        if keymap {
            let map = self.keyboard_map()?;
            if self.map.as_ref() != Some(&map) {
                self.map = Some(map);
                out.push(Event::Keymap);
            }
        }
        Ok(out)
    }
}

fn invisible_cursor(conn: &RustConnection, root: xproto::Window) -> Result<xproto::Cursor, Error> {
    let pixmap = conn.generate_id().map_err(x)?;
    conn.create_pixmap(1, pixmap, root, 1, 1).map_err(x)?;
    let gc = conn.generate_id().map_err(x)?;
    conn.create_gc(gc, pixmap, &xproto::CreateGCAux::new().foreground(0)).map_err(x)?;
    conn.poly_fill_rectangle(pixmap, gc, &[xproto::Rectangle { x: 0, y: 0, width: 1, height: 1 }])
        .map_err(x)?;
    let cursor = conn.generate_id().map_err(x)?;
    conn.create_cursor(cursor, pixmap, pixmap, 0, 0, 0, 0, 0, 0, 0, 0).map_err(x)?;
    conn.free_gc(gc).map_err(x)?;
    conn.free_pixmap(pixmap).map_err(x)?;
    Ok(cursor)
}
