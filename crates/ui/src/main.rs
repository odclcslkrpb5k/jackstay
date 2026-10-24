//! `jackstay-ui`: the history popup, shown when `jackstay popup` asks (docs/ui.md).

use std::cell::RefCell;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::glib;
use jackstay_ui::listen::{self, ListenError};
use jackstay_ui::popup::Popup;
use jackstay_ui::tray::{self, Action};
use jackstay_ui::worker;

/// Exit statuses, as the daemon's: 2 bad setup, 3 another UI running, 5 no display.
const BAD_SETUP: i32 = 2;
const RUNNING: i32 = 3;
const NO_DISPLAY: i32 = 5;

thread_local! {
    /// The popup, for work handed to the GTK thread from the others.
    static POPUP: RefCell<Option<Rc<Popup>>> = const { RefCell::new(None) };
}

fn log(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// The runtime directory: `--runtime-dir DIR`, else `$XDG_RUNTIME_DIR/jackstay`.
fn runtime(args: &[OsString]) -> Result<PathBuf, String> {
    match args {
        [] => std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .map(|p| p.join("jackstay"))
            .ok_or_else(|| "XDG_RUNTIME_DIR is not set".to_owned()),
        [flag, dir] if flag == "--runtime-dir" => Ok(PathBuf::from(dir)),
        _ => Err("usage: jackstay-ui [--runtime-dir DIR]".to_owned()),
    }
}

/// Runs `f` with the popup, on the GTK thread.
fn on_popup(f: impl FnOnce(&Popup) + Send + 'static) {
    glib::MainContext::default().invoke(move || {
        POPUP.with(|p| {
            if let Some(popup) = p.borrow().as_ref() {
                f(popup);
            }
        });
    });
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let runtime = match runtime(&args) {
        Ok(r) => r,
        Err(why) => {
            log(&why);
            std::process::exit(BAD_SETUP);
        }
    };
    if gtk::init().is_err() {
        log("no display");
        std::process::exit(NO_DISPLAY);
    }
    let listener = match listen::bind(&runtime) {
        Ok(l) => l,
        Err(ListenError::Running) => {
            log("another jackstay-ui is running");
            std::process::exit(RUNNING);
        }
        Err(ListenError::Io(e)) => {
            log(&format!("ui socket: {e}"));
            std::process::exit(BAD_SETUP);
        }
    };
    let jobs = match worker::spawn(runtime.join("control"), |done| on_popup(move |p| p.done(done)))
    {
        Ok(j) => j,
        Err(e) => {
            log(&format!("worker: {e}"));
            std::process::exit(BAD_SETUP);
        }
    };
    let popup = Popup::new(jobs);
    let tray_popup = Rc::clone(&popup);
    POPUP.with(|p| *p.borrow_mut() = Some(popup));
    // Without a session bus there is no tray; the popup still works from its hotkey.
    if let Err(e) = tray::start(move |action| match action {
        Action::ShowHistory => tray_popup.request(),
        Action::ClearClipboard => tray_popup.clear(),
        Action::ClearHistory => tray_popup.confirm_clear_history(),
    }) {
        log(&format!("no tray icon: {e}"));
    }
    if let Err(e) = listener.serve(|| on_popup(Popup::request)) {
        log(&format!("ui socket: {e}"));
        std::process::exit(BAD_SETUP);
    }
    gtk::main();
}
