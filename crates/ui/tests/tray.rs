//! The tray icon end to end: the real `jackstay-ui` on a private session bus and Xvfb, with a
//! stand-in for the panel's `StatusNotifierWatcher`, and `gdbus` playing the panel's clicks
//! (docs/ui.md, "The tray").

mod support;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use gtk::gio;
use gtk::glib::{self, prelude::ToVariant};
use jackstay_proto::control::Request;
use support::*;

static NEXT_BUS: AtomicU32 = AtomicU32::new(0);

/// A session bus of its own.
struct Bus {
    child: Child,
    address: String,
}

impl Bus {
    fn start() -> Self {
        // dbus-daemon will not start for a uid with no passwd entry, which is how CI runs the
        // container: nss_wrapper gives it one, for this process alone.
        let ids = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "bus-ids-{}-{}",
            std::process::id(),
            NEXT_BUS.fetch_add(1, Ordering::Relaxed)
        ));
        ok(std::fs::create_dir_all(&ids));
        let (uid, gid) = (rustix::process::getuid().as_raw(), rustix::process::getgid().as_raw());
        ok(std::fs::write(
            ids.join("passwd"),
            format!("tester:x:{uid}:{gid}:tester:/tmp:/bin/sh\n"),
        ));
        ok(std::fs::write(ids.join("group"), format!("tester:x:{gid}:\n")));
        let mut child = ok(Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .env("LD_PRELOAD", "libnss_wrapper.so")
            .env("NSS_WRAPPER_PASSWD", ids.join("passwd"))
            .env("NSS_WRAPPER_GROUP", ids.join("group"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn());
        let mut address = String::new();
        let _ = BufReader::new(child.stdout.take().unwrap_or_else(|| panic!("no stdout")))
            .read_line(&mut address);
        let address = address.trim().to_owned();
        assert!(address.starts_with("unix:"), "{address:?}");
        Bus { child, address }
    }

    /// `gdbus ARGS` on this bus; its output, if it succeeded.
    fn gdbus(&self, args: &[&str]) -> Option<String> {
        let out = ok(Command::new("gdbus")
            .args(args)
            .env("DBUS_SESSION_BUS_ADDRESS", &self.address)
            .output());
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const WATCHER_XML: &str = r#"<node><interface name="org.kde.StatusNotifierWatcher">
  <method name="RegisterStatusNotifierItem"><arg name="service" type="s" direction="in"/></method>
</interface></node>"#;

/// The panel's watcher, standing in: records each item that registers. Dropping it stops it,
/// as a panel restarting does.
struct Watcher {
    registered: Arc<Mutex<Vec<String>>>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    fn start(bus: &Bus, registered: Arc<Mutex<Vec<String>>>) -> Self {
        let (stop, stopped) = mpsc::channel::<()>();
        let (up, is_up) = mpsc::channel::<()>();
        let address = bus.address.clone();
        let log = Arc::clone(&registered);
        let thread = std::thread::spawn(move || {
            let context = glib::MainContext::new();
            let _ = context.with_thread_default(|| {
                let flags = gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
                    | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION;
                let bus = ok(gio::DBusConnection::for_address_sync(
                    &address,
                    flags,
                    None,
                    None::<&gio::Cancellable>,
                ));
                let info = ok(gio::DBusNodeInfo::for_xml(WATCHER_XML));
                let iface = info
                    .lookup_interface("org.kde.StatusNotifierWatcher")
                    .unwrap_or_else(|| panic!("no interface"));
                let id = ok(bus
                    .register_object("/StatusNotifierWatcher", &iface)
                    .method_call(move |_, _, _, _, _, args, invocation| {
                        let service = args.try_child_value(0).and_then(|v| v.get::<String>());
                        if let (Some(s), Ok(mut l)) = (service, log.lock()) {
                            l.push(s);
                        }
                        invocation.return_value(None);
                    })
                    .build());
                let owned = ok(bus.call_sync(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "RequestName",
                    Some(&("org.kde.StatusNotifierWatcher", 0u32).to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    5_000,
                    None::<&gio::Cancellable>,
                ));
                let _ = owned;
                let _ = up.send(());
                while stopped.try_recv() == Err(mpsc::TryRecvError::Empty) {
                    context.iteration(false);
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = bus.unregister_object(id);
                let _ = bus.close_sync(None::<&gio::Cancellable>);
            });
        });
        ok(is_up.recv_timeout(Duration::from_secs(10)));
        Watcher { registered, stop: Some(stop), thread: Some(thread) }
    }

    fn items(&self) -> Vec<String> {
        self.registered.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Tray {
    ui: Ui,
    bus: Bus,
    watcher: Option<Watcher>,
    registered: Arc<Mutex<Vec<String>>>,
}

fn tray(name: &str) -> Tray {
    let bus = Bus::start();
    let registered = Arc::new(Mutex::new(Vec::new()));
    let watcher = Watcher::start(&bus, Arc::clone(&registered));
    let address = bus.address.clone();
    let ui = start_with(name, |c| {
        c.env("DBUS_SESSION_BUS_ADDRESS", &address);
    });
    eventually("the item to register", || !watcher.items().is_empty());
    Tray { ui, bus, watcher: Some(watcher), registered }
}

impl Tray {
    fn service(&self) -> String {
        self.registered.lock().ok().and_then(|l| l.first().cloned()).unwrap_or_default()
    }

    fn call(&self, path: &str, method: &str, args: &[&str]) -> Option<String> {
        let service = self.service();
        let mut all = vec![
            "call",
            "--session",
            "--dest",
            &service,
            "--object-path",
            path,
            "--method",
            method,
        ];
        all.extend_from_slice(args);
        self.bus.gdbus(&all)
    }

    fn property(&self, path: &str, interface: &str, name: &str) -> String {
        self.call(path, "org.freedesktop.DBus.Properties.Get", &[interface, name])
            .unwrap_or_else(|| panic!("no {name}"))
    }

    fn click_menu(&self, id: i32) {
        let id = id.to_string();
        let ok =
            self.call("/MenuBar", "com.canonical.dbusmenu.Event", &[&id, "clicked", "<0>", "0"]);
        assert!(ok.is_some(), "menu item {id}");
    }

    fn dialog(&self) -> bool {
        let out = ok(self
            .ui
            .x
            .command("xdotool")
            .args(["search", "--onlyvisible", "--name", "^Clear history$"])
            .output());
        out.status.success()
    }
}

#[test]
fn the_item_registers_as_the_panel_expects_and_names_its_icon_and_menu() {
    let t = tray("props");
    let pid = t.ui.child.id();
    assert_eq!(t.service(), format!("org.kde.StatusNotifierItem-{pid}-1"));
    let item = "org.kde.StatusNotifierItem";
    assert_eq!(t.property("/StatusNotifierItem", item, "IconName"), "(<'jackstay'>,)\n");
    assert_eq!(t.property("/StatusNotifierItem", item, "Menu"), "(<objectpath '/MenuBar'>,)\n");
    assert_eq!(t.property("/StatusNotifierItem", item, "ItemIsMenu"), "(<false>,)\n");
    assert!(t.property("/StatusNotifierItem", item, "ToolTip").contains("'Jackstay'"));
    assert!(t.property("/StatusNotifierItem", item, "IconThemePath").contains("icons"));
    let layout =
        t.call("/MenuBar", "com.canonical.dbusmenu.GetLayout", &["0", "int32 -1", "@as []"]);
    let layout = layout.unwrap_or_default();
    for label in ["Show history", "Clear clipboard", "Clear history…", "Settings…", "separator"]
    {
        assert!(layout.contains(label), "{label} in {layout}");
    }
}

#[test]
fn a_click_on_the_icon_opens_the_popup() {
    let t = tray("activate");
    let _daemon = Daemon::start(&t.ui.runtime, history());
    assert!(
        t.call("/StatusNotifierItem", "org.kde.StatusNotifierItem.Activate", &["10", "10"])
            .is_some()
    );
    eventually("the popup", || t.ui.visible());
}

#[test]
fn the_menu_shows_history_clears_and_asks_before_clearing_history() {
    let t = tray("menu");
    let daemon = Daemon::start(&t.ui.runtime, history());
    t.click_menu(1);
    eventually("the popup", || t.ui.visible());
    t.ui.keys(&["Escape"]);
    t.ui.closed();
    t.click_menu(3);
    eventually("the clear", || daemon.changes() == [Request::Clear]);
    // Asked first; Cancel is the default.
    t.click_menu(4);
    eventually("the question", || t.dialog());
    t.ui.keys(&["Return"]);
    eventually("the question gone", || !t.dialog());
    t.click_menu(4);
    eventually("the question", || t.dialog());
    t.ui.keys(&["alt+l"]);
    eventually("history cleared", || daemon.changes() == [Request::Clear, Request::ClearHistory]);
    // A group of events, as some hosts send them.
    let group = t.call(
        "/MenuBar",
        "com.canonical.dbusmenu.EventGroup",
        &["[(3, 'clicked', <0>, uint32 0)]"],
    );
    assert!(group.is_some());
    eventually("cleared again", || daemon.changes().len() == 3);
    // Items that are not actions do nothing.
    t.click_menu(2);
    t.click_menu(99);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(daemon.changes().len(), 3);
    // Settings… opens the settings window.
    t.click_menu(6);
    eventually("the settings window", || {
        let out =
            t.ui.x
                .command("xdotool")
                .args(["search", "--onlyvisible", "--name", "Jackstay settings"])
                .output();
        out.is_ok_and(|o| o.status.success())
    });
}

#[test]
fn it_registers_again_when_the_panel_restarts() {
    let mut t = tray("restart");
    drop(t.watcher.take());
    let again = Arc::clone(&t.registered);
    t.watcher = Some(Watcher::start(&t.bus, again));
    eventually("registered twice", || t.registered.lock().map_or(0, |l| l.len()) == 2);
}

#[test]
fn without_a_session_bus_the_popup_still_works() {
    let ui = start_with("nobus", |c| {
        c.env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/bus");
    });
    let _daemon = Daemon::start(&ui.runtime, history());
    ui.popup();
}

/// Every property and method answers, with the type it declares: `GLib` drops a reply of any other
/// type, and the host then shows nothing.
#[test]
fn every_property_and_method_answers_with_its_declared_type() {
    let t = tray("types");
    let get_all = |path: &str, iface: &str| {
        t.call(path, "org.freedesktop.DBus.Properties.GetAll", &[iface, "--timeout", "3"])
            .unwrap_or_else(|| panic!("GetAll {iface}"))
    };
    let item = get_all("/StatusNotifierItem", "org.kde.StatusNotifierItem");
    for want in [
        "'Category': <'ApplicationStatus'>",
        "'Id': <'jackstay'>",
        "'Title': <'Jackstay'>",
        "'Status': <'Active'>",
        "'WindowId': <0>",
        "'IconName': <'jackstay'>",
        "'IconPixmap': <@a(iiay) []>",
        "'OverlayIconName': <''>",
        "'AttentionIconName': <''>",
        "'ItemIsMenu': <false>",
        "'Menu': <objectpath '/MenuBar'>",
    ] {
        assert!(item.contains(want), "{want} in {item}");
    }
    let menu = get_all("/MenuBar", "com.canonical.dbusmenu");
    for want in [
        "'Version': <uint32 3>",
        "'TextDirection': <'ltr'>",
        "'Status': <'normal'>",
        "'IconThemePath': <@as []>",
    ] {
        assert!(menu.contains(want), "{want} in {menu}");
    }
    let call = |method: &str, args: &[&str]| {
        let mut all = args.to_vec();
        all.extend_from_slice(&["--timeout", "3"]);
        t.call("/MenuBar", &format!("com.canonical.dbusmenu.{method}"), &all)
            .unwrap_or_else(|| panic!("{method} did not answer"))
    };
    let props = call("GetGroupProperties", &["[1, 2]", "@as []"]);
    assert!(props.contains("'Show history'") && props.contains("'separator'"), "{props}");
    assert_eq!(call("GetProperty", &["1", "label"]), "(<'Show history'>,)\n");
    assert_eq!(call("GetProperty", &["99", "children-display"]), "(<''>,)\n");
    assert_eq!(call("GetProperty", &["0", "children-display"]), "(<'submenu'>,)\n");
    assert_eq!(call("AboutToShow", &["0"]), "(false,)\n");
    assert_eq!(call("AboutToShowGroup", &["[0]"]), "(@ai [], @ai [])\n");
}
