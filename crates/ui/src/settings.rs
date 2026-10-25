//! The settings window (docs/ui.md, "Settings"): the history's and capture's limits, which the
//! daemon reads from `config.toml` as it starts, the popup's hotkey, and starting at login.
//! Nothing changes until Save; reading and saving run on a thread of their own (`desktop`).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};

use gtk::gdk::keys::constants as k;
use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::desktop::{self, Changes, Loaded, Places};
use crate::form::{self, Fields, RANGES};

/// The title, which also names the window to tests and `xdotool`.
pub const TITLE: &str = "Jackstay settings";

const LIMITS_NOTE: &str = "Saving restarts jackstayd to apply these. Lowering a limit drops the \
                           oldest entries beyond it; lowering the largest item kept drops saved \
                           entries bigger than it.";
const PRESS: &str = "Press the keys… (Esc cancels)";

/// What the window asks of its thread.
#[derive(Debug)]
pub enum Task {
    /// Reads the settings; `show` opens the window once read.
    Load {
        show: bool,
    },
    Save(Box<Loaded>, Changes),
}

/// What came of a task.
#[derive(Debug)]
pub enum Finished {
    Loaded {
        loaded: Box<Loaded>,
        show: bool,
    },
    /// What went wrong, one line each, or nothing.
    Saved(Vec<String>),
}

/// Starts the thread; each task's result is handed to `deliver`, on that thread.
///
/// # Errors
/// The thread could not be started.
pub fn spawn(
    places: Places,
    deliver: impl Fn(Finished) + Send + 'static,
) -> std::io::Result<Sender<Task>> {
    let (tasks, queue) = mpsc::channel::<Task>();
    std::thread::Builder::new().name("settings".to_owned()).spawn(move || {
        for task in queue {
            deliver(match task {
                Task::Load { show } => {
                    Finished::Loaded { loaded: Box::new(desktop::load(&places)), show }
                }
                Task::Save(loaded, changes) => {
                    Finished::Saved(desktop::save(&places, &loaded, &changes))
                }
            });
        }
    })?;
    Ok(tasks)
}

/// The hotkey the window was asked for.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum Asked {
    #[default]
    Unchanged,
    /// These keys, or none to remove it.
    Keys(Option<String>),
}

#[derive(Debug, Default)]
struct State {
    loaded: Option<Loaded>,
    /// The hotkey as shown: the first of those bound.
    hotkey: Option<String>,
    /// The keys asked for (none to remove it), once changed: only then is the hotkey saved, so a
    /// save of something else never touches the bindings (there may be several, from
    /// `tools/install`).
    asked: Asked,
    capturing: bool,
    /// Asked to show; shown once read.
    pending: bool,
    saving: bool,
}

#[derive(Debug)]
pub struct Settings {
    window: gtk::Window,
    entries: gtk::SpinButton,
    size: gtk::SpinButton,
    pinned: gtk::SpinButton,
    target: gtk::SpinButton,
    per_copy: gtk::SpinButton,
    hotkey: gtk::Label,
    change: gtk::Button,
    unbind: gtk::Button,
    login: gtk::CheckButton,
    status: gtk::Label,
    save: gtk::Button,
    places: Places,
    tasks: Sender<Task>,
    state: RefCell<State>,
}

/// A spin button for whole numbers in `range`.
fn spin(range: (u64, u64)) -> gtk::SpinButton {
    let s = gtk::SpinButton::with_range(float(range.0), float(range.1), 1.0);
    s.set_digits(0);
    s.set_numeric(true);
    s.set_activates_default(true);
    s
}

/// `n` as a float, exactly for any field's value (all below 2^53), without a lossy cast.
fn float(n: u64) -> f64 {
    let high = u32::try_from(n >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(n & 0xFFFF_FFFF).unwrap_or(0);
    f64::from(high) * 4_294_967_296.0 + f64::from(low)
}

/// A spin button's whole value.
fn whole(s: &gtk::SpinButton) -> u64 {
    s.update();
    // Shown with no digits, so its text is the whole number.
    format!("{:.0}", s.value()).parse().unwrap_or(0)
}

/// A labelled row of `grid`.
fn row(grid: &gtk::Grid, at: i32, label: &str, widget: &impl IsA<gtk::Widget>, unit: &str) {
    let l = gtk::Label::with_mnemonic(label);
    l.set_xalign(0.0);
    l.set_mnemonic_widget(Some(widget));
    grid.attach(&l, 0, at, 1, 1);
    grid.attach(widget, 1, at, 1, 1);
    if !unit.is_empty() {
        let u = gtk::Label::new(Some(unit));
        u.set_xalign(0.0);
        grid.attach(&u, 2, at, 1, 1);
    }
}

fn section(title: &str) -> (gtk::Frame, gtk::Grid) {
    let frame = gtk::Frame::new(None);
    let label = gtk::Label::new(None);
    label.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(title)));
    frame.set_label_widget(Some(&label));
    frame.set_shadow_type(gtk::ShadowType::None);
    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(12);
    grid.set_margin_start(12);
    grid.set_margin_top(6);
    frame.add(&grid);
    (frame, grid)
}

/// What the hotkey `keys` is shown as.
fn key_label(keys: Option<&str>) -> String {
    let Some(keys) = keys else { return "None".to_owned() };
    let (key, mods) = gtk::accelerator_parse(keys);
    gtk::accelerator_get_label(key, mods)
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| keys.to_owned())
}

/// The modifiers a hotkey of `keyval` pressed with `state` is named with, or `None` if it would
/// take a key people type: a hotkey needs Ctrl, Alt or Super, unless it is a function key.
#[must_use]
pub fn hotkey_modifiers(keyval: u32, state: gdk::ModifierType) -> Option<gdk::ModifierType> {
    use gdk::ModifierType as M;
    // The default accelerator mask (`gtk_accelerator_get_default_mod_mask`), which needs no
    // display. X reports Super as Mod4, which that mask leaves out; Xfce names it <Super>.
    let mut mods = state
        & (M::CONTROL_MASK
            | M::SHIFT_MASK
            | M::MOD1_MASK
            | M::SUPER_MASK
            | M::HYPER_MASK
            | M::META_MASK);
    if state.contains(M::MOD4_MASK) {
        mods |= M::SUPER_MASK;
    }
    let function = (*k::F1..=*k::F35).contains(&keyval);
    (function || mods.intersects(M::CONTROL_MASK | M::MOD1_MASK | M::SUPER_MASK)).then_some(mods)
}

/// Whether two of Xfce's key names mean the same keys (`<Primary>` and `<Control>` do).
fn same_keys(a: &str, b: &str) -> bool {
    let (a, b) = (gtk::accelerator_parse(a), gtk::accelerator_parse(b));
    a.0 != 0 && a == b
}

impl Settings {
    #[must_use]
    pub fn new(places: Places, tasks: Sender<Task>) -> Rc<Self> {
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title(TITLE);
        window.set_icon_name(Some(crate::tray::ICON));
        window.set_resizable(false);
        window.set_position(gtk::WindowPosition::Center);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 12);
        column.set_border_width(12);

        let (history, grid) = section("History");
        let entries = spin(RANGES.entries);
        let size = spin(RANGES.size_mib);
        let pinned = spin(RANGES.pinned);
        row(&grid, 0, "_Entries kept", &entries, "unpinned");
        row(&grid, 1, "Si_ze kept", &size, "MiB, unpinned");
        row(&grid, 2, "_Pinned entries", &pinned, "at most");
        let (capture, grid) = section("Copying");
        let target = spin(RANGES.target_mib);
        let per_copy = spin(RANGES.entry_mib);
        row(&grid, 0, "Largest _item kept", &target, "MiB");
        row(&grid, 1, "_Most kept from one copy", &per_copy, "MiB");
        let note = gtk::Label::new(Some(LIMITS_NOTE));
        note.set_xalign(0.0);
        note.set_line_wrap(true);
        note.set_max_width_chars(56);
        note.style_context().add_class("dim-label");

        let (desktop, grid) = section("Desktop");
        let keys = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let hotkey = gtk::Label::new(None);
        hotkey.set_xalign(0.0);
        hotkey.set_width_chars(16);
        let change = gtk::Button::with_mnemonic("Chan_ge…");
        let unbind = gtk::Button::with_mnemonic("_Remove");
        keys.pack_start(&hotkey, true, true, 0);
        keys.pack_start(&change, false, false, 0);
        keys.pack_start(&unbind, false, false, 0);
        row(&grid, 0, "History hot_key", &keys, "");
        let login = gtk::CheckButton::with_mnemonic("Start at _login");
        grid.attach(&login, 0, 1, 2, 1);

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_line_wrap(true);
        status.set_max_width_chars(56);
        let buttons = gtk::ButtonBox::new(gtk::Orientation::Horizontal);
        buttons.set_layout(gtk::ButtonBoxStyle::End);
        buttons.set_spacing(6);
        let cancel = gtk::Button::with_mnemonic("_Cancel");
        let save = gtk::Button::with_mnemonic("_Save");
        save.set_can_default(true);
        buttons.add(&cancel);
        buttons.add(&save);
        for w in [history.upcast_ref::<gtk::Widget>(), capture.upcast_ref(), note.upcast_ref()] {
            column.pack_start(w, false, false, 0);
        }
        column.pack_start(&desktop, false, false, 0);
        column.pack_start(&status, false, false, 0);
        column.pack_end(&buttons, false, false, 0);
        window.add(&column);
        save.grab_default();

        let settings = Rc::new(Settings {
            window,
            entries,
            size,
            pinned,
            target,
            per_copy,
            hotkey,
            change,
            unbind,
            login,
            status,
            save,
            places,
            tasks,
            state: RefCell::new(State::default()),
        });
        let s = Rc::clone(&settings);
        settings.change.connect_clicked(move |_| s.capture());
        let s = Rc::clone(&settings);
        settings.unbind.connect_clicked(move |_| s.set_hotkey(None));
        let s = Rc::clone(&settings);
        settings.save.connect_clicked(move |_| s.save());
        let s = Rc::clone(&settings);
        cancel.connect_clicked(move |_| s.close());
        let s = Rc::clone(&settings);
        settings.window.connect_key_press_event(move |_, key| s.key(key));
        let s = Rc::clone(&settings);
        settings.window.connect_delete_event(move |_, _| {
            s.close();
            glib::Propagation::Stop
        });
        settings
    }

    /// `jackstay settings`, or the tray's item: reads the settings, and shows them once read.
    pub fn request(&self) {
        if self.window.is_visible() {
            self.window.present();
            return;
        }
        let pending = std::mem::replace(&mut self.state.borrow_mut().pending, true);
        if !pending {
            self.send(Task::Load { show: true });
        }
    }

    /// A task's result, on the GTK thread.
    pub fn finished(&self, finished: Finished) {
        match finished {
            Finished::Loaded { loaded, show } => {
                let asked = show && std::mem::take(&mut self.state.borrow_mut().pending);
                if asked && !self.window.is_visible() {
                    self.fill(*loaded);
                    self.window.show_all();
                } else {
                    // Already showing (after a save that partly failed, or asked for during
                    // one): what is now so is kept for the next save, and the fields and the
                    // status keep what was asked and said.
                    self.state.borrow_mut().loaded = Some(*loaded);
                }
                if asked {
                    self.window.present();
                }
                if !show {
                    // The reread after a failed save: saving again now starts from it.
                    self.state.borrow_mut().saving = false;
                    self.save.set_sensitive(true);
                }
            }
            Finished::Saved(problems) => {
                if problems.is_empty() {
                    self.state.borrow_mut().saving = false;
                    self.save.set_sensitive(true);
                    self.close();
                } else {
                    // Shown again if closed meanwhile, so what did not happen is seen; Save
                    // waits for the settings to be read again.
                    self.status.set_text(&problems.join("\n"));
                    self.window.show_all();
                    self.window.present();
                    self.send(Task::Load { show: false });
                }
            }
        }
    }

    fn send(&self, task: Task) {
        // The thread lives as long as the process; a send can only fail as it exits.
        let _ = self.tasks.send(task);
    }

    fn fill(&self, loaded: Loaded) {
        let f = form::fields(&loaded.config);
        for (spin, value) in [
            (&self.entries, f.entries),
            (&self.size, f.size_mib),
            (&self.pinned, f.pinned),
            (&self.target, f.target_mib),
            (&self.per_copy, f.entry_mib),
        ] {
            spin.set_value(float(value));
        }
        self.status.set_text(&loaded.config_problem.as_ref().map_or_else(String::new, |why| {
            format!("config.toml could not be used ({why}); saving replaces it.")
        }));
        let hotkeys = loaded.hotkeys(&self.places);
        let readable = hotkeys.is_some();
        for w in [&self.change, &self.unbind] {
            w.set_sensitive(readable);
        }
        self.hotkey.set_tooltip_text((!readable).then_some("xfconf-query could not be run"));
        self.login.set_active(loaded.login == Some(true));
        self.login.set_sensitive(loaded.login.is_some());
        self.login.set_tooltip_text(loaded.login.is_none().then_some(
            "No autostart entry (tools/install writes ~/.config/autostart/jackstay.desktop)",
        ));
        {
            let mut s = self.state.borrow_mut();
            s.hotkey = hotkeys.and_then(|h| h.into_iter().next());
            s.asked = Asked::Unchanged;
            s.capturing = false;
            s.loaded = Some(loaded);
        }
        self.show_hotkey();
        self.entries.grab_focus();
    }

    fn show_hotkey(&self) {
        let s = self.state.borrow();
        if s.capturing {
            self.hotkey.set_text(PRESS);
        } else if s.loaded.as_ref().is_some_and(|l| l.shortcuts.is_none()) {
            self.hotkey.set_text("Unknown");
        } else {
            self.hotkey.set_text(&key_label(s.hotkey.as_deref()));
        }
    }

    fn capture(&self) {
        self.state.borrow_mut().capturing = true;
        self.status.set_text("");
        self.show_hotkey();
    }

    fn set_hotkey(&self, keys: Option<String>) {
        {
            let mut s = self.state.borrow_mut();
            s.hotkey.clone_from(&keys);
            s.asked = Asked::Keys(keys);
            s.capturing = false;
        }
        self.show_hotkey();
    }

    fn stop_capturing(&self) {
        self.state.borrow_mut().capturing = false;
        self.show_hotkey();
    }

    /// The keys pressed while capturing: the hotkey, if they make one.
    fn captured(&self, key: &gdk::EventKey) {
        if key.is_modifier() {
            return;
        }
        let keyval = key.keyval();
        // Esc alone stops; with a modifier it can be a hotkey.
        if keyval == k::Escape && (key.state() & gtk::accelerator_get_default_mod_mask()).is_empty()
        {
            self.stop_capturing();
            return;
        }
        let Some(mods) = hotkey_modifiers(*keyval, key.state()) else {
            self.status.set_text("A hotkey needs Ctrl, Alt or Super (or is a function key).");
            return;
        };
        let lower = *keyval.to_lower();
        let Some(name) =
            gtk::accelerator_name(lower, mods).filter(|_| gtk::accelerator_valid(lower, mods))
        else {
            return;
        };
        let taken = self.state.borrow().loaded.as_ref().and_then(|l| {
            let shortcuts = l.shortcuts.as_deref()?;
            desktop::taken(shortcuts, &name, &self.places.popup_command, same_keys)
        });
        if let Some(what) = taken {
            self.status.set_text(&format!("{} already {what}.", key_label(Some(&name))));
        } else {
            self.status.set_text("");
            self.set_hotkey(Some(name.to_string()));
        }
    }

    fn key(&self, key: &gdk::EventKey) -> glib::Propagation {
        if self.state.borrow().capturing {
            self.captured(key);
            return glib::Propagation::Stop;
        }
        if key.keyval() == k::Escape {
            self.close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    }

    /// What Save would change, or why it cannot.
    fn changes(&self) -> Result<Changes, String> {
        let s = self.state.borrow();
        let loaded = s.loaded.as_ref().ok_or("The settings have not been read.")?;
        let asked = Fields {
            entries: whole(&self.entries),
            size_mib: whole(&self.size),
            pinned: whole(&self.pinned),
            target_mib: whole(&self.target),
            entry_mib: whole(&self.per_copy),
        };
        let config = form::config(&asked, &loaded.config)?;
        let mut changes = Changes::default();
        if config != loaded.config || loaded.config_problem.is_some() {
            changes.config = Some(config);
        }
        let bound = loaded.hotkeys(&self.places);
        if let (Asked::Keys(asked), Some(bound)) = (&s.asked, bound)
            && bound != asked.iter().cloned().collect::<Vec<_>>()
        {
            changes.hotkey = Some(asked.clone());
        }
        if loaded.login.is_some_and(|on| on != self.login.is_active()) {
            changes.login = Some(self.login.is_active());
        }
        Ok(changes)
    }

    fn save(&self) {
        if self.state.borrow().saving {
            return;
        }
        let changes = match self.changes() {
            Ok(c) => c,
            Err(why) => {
                self.status.set_text(&why);
                return;
            }
        };
        if changes == Changes::default() {
            self.close();
            return;
        }
        let Some(loaded) = self.state.borrow().loaded.clone() else { return };
        self.state.borrow_mut().saving = true;
        self.save.set_sensitive(false);
        self.status.set_text("Saving…");
        self.send(Task::Save(Box::new(loaded), changes));
    }

    fn close(&self) {
        {
            let mut s = self.state.borrow_mut();
            s.capturing = false;
        }
        self.window.hide();
    }
}
