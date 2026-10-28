//! The history popup (docs/ui.md, "The popup"): a window at the pointer listing the history,
//! newest first, with a search field. It takes the keyboard and pointer itself, since a popup
//! window gets no focus from the window manager.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use gtk::gdk::keys::constants as k;
use gtk::prelude::*;
use gtk::{gdk, glib};
use jackstay_proto::control::{self, Summary, Thumb, Thumbnail};

use crate::model::{self, Row};
use crate::worker::{Done, Job};

/// How long to keep trying to take the keyboard: a hotkey's own grab may still be held for a
/// moment after it fires.
const GRAB_FOR: Duration = Duration::from_secs(1);
const GRAB_EVERY: Duration = Duration::from_millis(25);

/// The title, which also names the window to tests and `xdotool`.
pub const TITLE: &str = "Jackstay history";

/// The tray's confirmation dialog's title, which also names it to tests.
pub const CLEAR_HISTORY_TITLE: &str = "Clear history";

const HINT: &str = "Enter: paste   Ctrl+P: pin   Ctrl+D: delete   Esc: close";

/// How long before a thumbnail still being made is asked for again, while the popup is open: this
/// times the tries so far, at most [`THUMBNAIL_MAX_WAIT`]; and the most tries per opening.
const THUMBNAIL_EVERY: Duration = Duration::from_millis(200);
const THUMBNAIL_MAX_WAIT: Duration = Duration::from_secs(1);
const THUMBNAIL_TRIES: u32 = 60;

/// What the popup has of an entry's thumbnail.
#[derive(Debug)]
struct Known {
    /// The entry's size when asked: a different size is different content, asked again.
    size: u64,
    thumb: Thumb,
    tries: u32,
    /// Asked and not yet answered, or to be asked again: never two at once.
    asking: bool,
}

/// The widgets of an image entry's row, to fill in when its thumbnail arrives.
#[derive(Debug)]
struct Shown {
    image: gtk::Image,
    title: gtk::Label,
}

#[derive(Debug, Default)]
struct State {
    entries: Vec<Summary>,
    total: u32,
    /// Indexes into `entries` that match the search, in order.
    shown: Vec<usize>,
    /// Asked to show; shown once the list arrives.
    pending: bool,
    open: bool,
    /// Clear history pressed once, waiting for the second press.
    confirm_clear: bool,
    /// Thumbnails of the listed entries with images, by id.
    thumbnails: HashMap<u64, Known>,
    /// The rows shown now that hold an image, by id.
    images: HashMap<u64, Shown>,
}

#[derive(Debug)]
pub struct Popup {
    window: gtk::Window,
    search: gtk::SearchEntry,
    list: gtk::ListBox,
    status: gtk::Label,
    clear_history: gtk::Button,
    jobs: Sender<Job>,
    /// Thumbnails go through a connection of their own, so a paste never waits behind them.
    thumbnail_jobs: Sender<Job>,
    state: RefCell<State>,
}

impl Popup {
    #[must_use]
    pub fn new(jobs: Sender<Job>, thumbnail_jobs: Sender<Job>) -> Rc<Self> {
        let window = gtk::Window::new(gtk::WindowType::Popup);
        window.set_title(TITLE);
        window.set_default_size(520, -1);
        let frame = gtk::Frame::new(None);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
        column.set_border_width(8);
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search"));
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Browse);
        list.set_activate_on_single_click(true);
        let scroll = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_max_content_height(420);
        scroll.set_propagate_natural_height(true);
        scroll.add(&list);
        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_line_wrap(true);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let clear = gtk::Button::with_mnemonic("_Clear clipboard");
        let clear_history = gtk::Button::with_mnemonic("Clear _history");
        buttons.pack_start(&clear, false, false, 0);
        buttons.pack_start(&clear_history, false, false, 0);
        let hint = gtk::Label::new(Some(HINT));
        hint.set_xalign(0.0);
        hint.style_context().add_class("dim-label");
        column.pack_start(&search, false, false, 0);
        column.pack_start(&scroll, true, true, 0);
        column.pack_start(&status, false, false, 0);
        column.pack_start(&buttons, false, false, 0);
        column.pack_start(&hint, false, false, 0);
        frame.add(&column);
        window.add(&frame);

        let popup = Rc::new(Popup {
            window,
            search,
            list,
            status,
            clear_history,
            jobs,
            thumbnail_jobs,
            state: RefCell::new(State::default()),
        });
        let p = Rc::clone(&popup);
        // `changed`, not `search-changed`: that one waits 150 ms after the last key, and an Enter
        // pressed sooner would paste from the list before it was filtered.
        popup.search.connect_changed(move |_| p.refill());
        let p = Rc::clone(&popup);
        popup.list.connect_row_activated(move |_, row| p.choose(usize::try_from(row.index()).ok()));
        let p = Rc::clone(&popup);
        clear.connect_clicked(move |_| p.clear());
        let p = Rc::clone(&popup);
        popup.clear_history.connect_clicked(move |_| p.clear_history_pressed());
        let p = Rc::clone(&popup);
        popup.window.connect_key_press_event(move |_, key| p.key(key));
        let p = Rc::clone(&popup);
        popup.window.connect_button_press_event(move |w, event| {
            // With the pointer grabbed, a click anywhere comes here; one outside closes.
            let (x, y) = event.position();
            let (width, height) = (f64::from(w.allocated_width()), f64::from(w.allocated_height()));
            if x < 0.0 || y < 0.0 || x >= width || y >= height {
                p.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        popup
    }

    /// `jackstay popup`: lists the history, and shows it at the pointer once listed.
    pub fn request(&self) {
        let open = self.state.borrow().open;
        if open {
            return;
        }
        self.state.borrow_mut().pending = true;
        self.send(Job::List);
    }

    /// A job's result, on the GTK thread.
    pub fn done(&self, done: Done) {
        match done {
            Done::Listed { total, entries } => {
                {
                    let mut s = self.state.borrow_mut();
                    s.entries = entries;
                    s.total = total;
                }
                self.ask_thumbnails();
                self.status.set_text("");
                let show = std::mem::take(&mut self.state.borrow_mut().pending);
                if show {
                    self.open();
                } else {
                    self.refill();
                }
            }
            // A change made from the popup while it stays open: list again.
            Done::Did(job @ (Job::Delete(_) | Job::Pin(..))) => {
                if let Job::Delete(id) = job {
                    self.forget(Some(id));
                }
                if self.state.borrow().open {
                    self.send(Job::List);
                }
            }
            Done::Thumbnail(id, thumb) => self.thumbnail(id, thumb),
            // What was cleared is not kept here either: its pixels are forgotten.
            Done::Did(Job::ClearHistory) => self.forget(None),
            Done::Did(Job::Clear) => {
                let current = self.state.borrow().entries.iter().find(|e| e.current).map(|e| e.id);
                if let Some(id) = current {
                    self.forget(Some(id));
                }
            }
            Done::Did(_) => {}
            // Not had this time (the daemon restarting, say): asked again when next listed.
            Done::Failed(Job::Thumbnail(id), _) => {
                self.state.borrow_mut().thumbnails.remove(&id);
            }
            Done::Failed(job, why) => {
                self.status.set_text(&why);
                let pending = std::mem::take(&mut self.state.borrow_mut().pending);
                let open = self.state.borrow().open;
                if job == Job::List && pending {
                    // Shown anyway, empty, so the reason is seen.
                    self.state.borrow_mut().entries.clear();
                    self.open();
                } else if job != Job::List && !open {
                    // A paste or a clear asked as the popup closed did not happen: open it again
                    // to say why, rather than leave the clipboard silently unchanged.
                    self.open();
                }
            }
        }
    }

    /// Asks for the thumbnail of each listed image entry not known, or known as other content, top
    /// first; forgets those of entries no longer listed.
    fn ask_thumbnails(&self) {
        let mut ask = Vec::new();
        {
            let mut s = self.state.borrow_mut();
            let s = &mut *s;
            let listed: Vec<(u64, u64)> =
                s.entries.iter().filter(|e| model::has_image(e)).map(|e| (e.id, e.size)).collect();
            s.thumbnails.retain(|id, k| listed.contains(&(*id, k.size)));
            for &(id, size) in &listed {
                let known = s.thumbnails.entry(id).or_insert(Known {
                    size,
                    thumb: Thumb::Pending,
                    tries: 0,
                    asking: false,
                });
                if known.thumb == Thumb::Pending && !known.asking {
                    known.tries = 0;
                    known.asking = true;
                    ask.push(id);
                }
            }
        }
        for id in ask {
            let _ = self.thumbnail_jobs.send(Job::Thumbnail(id));
        }
    }

    /// Forgets entry `id`'s thumbnail, or every one.
    fn forget(&self, id: Option<u64>) {
        let mut s = self.state.borrow_mut();
        match id {
            Some(id) => drop(s.thumbnails.remove(&id)),
            None => s.thumbnails.clear(),
        }
    }

    /// A thumbnail asked for: shown if its row is, or asked again soon if still being made and the
    /// popup open. One given up on stays pending, asked again at the next listing.
    fn thumbnail(&self, id: u64, thumb: Thumb) {
        let open = self.state.borrow().open;
        let wait = {
            let mut s = self.state.borrow_mut();
            let Some(known) = s.thumbnails.get_mut(&id) else { return };
            known.tries = known.tries.saturating_add(1);
            known.thumb = thumb;
            let again = known.thumb == Thumb::Pending && open && known.tries < THUMBNAIL_TRIES;
            known.asking = again;
            again.then(|| THUMBNAIL_EVERY.saturating_mul(known.tries).min(THUMBNAIL_MAX_WAIT))
        };
        if let Some(wait) = wait {
            let jobs = self.thumbnail_jobs.clone();
            glib::timeout_add_local_once(wait, move || {
                let _ = jobs.send(Job::Thumbnail(id));
            });
            return;
        }
        self.show_thumbnail(id);
    }

    /// Fills in entry `id`'s row with its thumbnail, if both are there.
    fn show_thumbnail(&self, id: u64) {
        let s = self.state.borrow();
        let (Some(shown), Some(Known { thumb: Thumb::Ready(t), .. })) =
            (s.images.get(&id), s.thumbnails.get(&id))
        else {
            return;
        };
        if let Some(entry) = s.entries.iter().find(|e| e.id == id) {
            let row = model::row(entry, Some((t.image_width(), t.image_height())));
            set_title(&shown.title, &row);
        }
        if let Some(pixbuf) = pixbuf(t) {
            shown.image.set_from_pixbuf(Some(&pixbuf));
        }
    }

    fn send(&self, job: Job) {
        // The worker lives as long as the process; a send can only fail as it exits.
        let _ = self.jobs.send(job);
    }

    fn open(&self) {
        self.search.set_text("");
        {
            let mut s = self.state.borrow_mut();
            s.open = true;
            s.confirm_clear = false;
        }
        self.clear_history.set_label("Clear _history");
        self.refill();
        self.place();
        self.window.show_all();
        self.window.present();
        self.search.grab_focus();
        grab(&self.window, GRAB_FOR);
    }

    fn close(&self) {
        if !std::mem::replace(&mut self.state.borrow_mut().open, false) {
            return;
        }
        if let Some(seat) = self.window.window().and_then(|w| w.display().default_seat()) {
            seat.ungrab();
        }
        self.window.hide();
    }

    /// At the pointer, kept within its monitor's work area.
    fn place(&self) {
        let Some(display) = gdk::Display::default() else { return };
        let Some(pointer) = display.default_seat().and_then(|s| s.pointer()) else { return };
        let (_, x, y) = pointer.position();
        let (_, natural) = self.window.preferred_size();
        let (width, height) = (natural.width.max(520), natural.height);
        let area = display.monitor_at_point(x, y).map(|m| m.workarea());
        let (x, y) = match area {
            Some(a) => (
                x.min(a.x() + a.width() - width).max(a.x()),
                y.min(a.y() + a.height() - height).max(a.y()),
            ),
            None => (x, y),
        };
        self.window.move_(x, y);
    }

    /// The rows that match the search, the first selected.
    fn refill(&self) {
        for child in self.list.children() {
            self.list.remove(&child);
        }
        let query = self.search.text();
        let shown: Vec<usize> = {
            let s = self.state.borrow();
            s.entries
                .iter()
                .enumerate()
                .filter(|(_, e)| model::matches(e, &query))
                .map(|(i, _)| i)
                .collect()
        };
        let mut images = HashMap::new();
        for &i in &shown {
            let entry = self.state.borrow().entries.get(i).cloned();
            if let Some(entry) = entry {
                let (widget, image) =
                    row_widget(&model::row(&entry, None), model::has_image(&entry));
                if let Some(image) = image {
                    images.insert(entry.id, image);
                }
                self.list.add(&widget);
            }
        }
        if shown.is_empty() && self.status.text().is_empty() {
            let none =
                if self.state.borrow().entries.is_empty() { "No history" } else { "No match" };
            self.list.add(&gtk::Label::new(Some(none)));
        }
        let first = !shown.is_empty();
        let ids: Vec<u64> = images.keys().copied().collect();
        {
            let mut s = self.state.borrow_mut();
            s.shown = shown;
            s.images = images;
        }
        self.list.show_all();
        for id in ids {
            self.show_thumbnail(id);
        }
        if first {
            self.list.select_row(self.list.row_at_index(0).as_ref());
        }
    }

    fn selected(&self) -> Option<usize> {
        self.list.selected_row().and_then(|r| usize::try_from(r.index()).ok())
    }

    fn entry_at(&self, row: Option<usize>) -> Option<Summary> {
        let s = self.state.borrow();
        row.and_then(|r| s.shown.get(r)).and_then(|&i| s.entries.get(i)).cloned()
    }

    /// Puts the entry at `row` on the clipboard, and closes.
    fn choose(&self, row: Option<usize>) {
        if let Some(entry) = self.entry_at(row) {
            self.send(Job::Select(entry.id));
            self.close();
        }
    }

    /// Clears the clipboard, and closes the popup if it is open.
    pub fn clear(&self) {
        self.send(Job::Clear);
        self.close();
    }

    /// Asks, in a dialog, whether to clear the history (from the tray, where a second press
    /// cannot ask); clears it if so. Cancel is the default.
    pub fn confirm_clear_history(self: &Rc<Self>) {
        let dialog = gtk::MessageDialog::new(
            None::<&gtk::Window>,
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Question,
            gtk::ButtonsType::None,
            "Clear the clipboard and every unpinned entry?",
        );
        dialog.set_title(CLEAR_HISTORY_TITLE);
        dialog.set_secondary_text(Some("Pinned entries are kept."));
        dialog.add_button("_Cancel", gtk::ResponseType::Cancel);
        dialog.add_button("C_lear history", gtk::ResponseType::Accept);
        dialog.set_default_response(gtk::ResponseType::Cancel);
        let p = Rc::clone(self);
        dialog.connect_response(move |d, response| {
            if response == gtk::ResponseType::Accept {
                p.send(Job::ClearHistory);
            }
            d.close();
        });
        dialog.show_all();
        dialog.present();
    }

    fn clear_history_pressed(&self) {
        let confirm = std::mem::replace(&mut self.state.borrow_mut().confirm_clear, true);
        if confirm {
            self.send(Job::ClearHistory);
            self.close();
        } else {
            let unpinned = self.state.borrow().entries.iter().filter(|e| !e.pinned).count();
            let more = self.state.borrow().total as usize > self.state.borrow().entries.len();
            let count = if more {
                "every unpinned entry".to_owned()
            } else {
                format!("{unpinned} entries")
            };
            self.clear_history.set_label(&format!("Clear {count}? Press again"));
        }
    }

    fn key(&self, key: &gdk::EventKey) -> glib::Propagation {
        let ctrl = key.state().contains(gdk::ModifierType::CONTROL_MASK);
        // The buttons' mnemonics, which GTK does not activate in a popup window.
        let alt = key.state().contains(gdk::ModifierType::MOD1_MASK);
        let len = self.state.borrow().shown.len();
        let move_by = |by: isize| {
            if let Some(to) = model::step(self.selected(), by, len) {
                let row = self.list.row_at_index(i32::try_from(to).unwrap_or(0));
                self.list.select_row(row.as_ref());
                if let Some(row) = row {
                    row.grab_focus();
                    self.search.grab_focus_without_selecting();
                }
            }
        };
        match key.keyval() {
            k::Escape => self.close(),
            k::Down => move_by(1),
            k::Up => move_by(-1),
            k::Page_Down => move_by(10),
            k::Page_Up => move_by(-10),
            k::Return | k::KP_Enter => self.choose(self.selected()),
            k::d | k::D if ctrl => {
                if let Some(entry) = self.entry_at(self.selected()) {
                    self.send(Job::Delete(entry.id));
                }
            }
            k::c | k::C if alt => self.clear(),
            k::h | k::H if alt => self.clear_history_pressed(),
            k::p | k::P if ctrl => {
                if let Some(entry) = self.entry_at(self.selected()) {
                    self.send(Job::Pin(entry.id, !entry.pinned));
                }
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }
}

/// A row, and if it holds an image, the widgets its thumbnail fills in once it arrives.
fn row_widget(row: &Row, image: bool) -> (gtk::ListBoxRow, Option<Shown>) {
    let widget = gtk::ListBoxRow::new();
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    line.set_border_width(3);
    let title = gtk::Label::new(None);
    set_title(&title, row);
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let detail = gtk::Label::new(Some(&row.detail));
    detail.style_context().add_class("dim-label");
    let shown = image.then(|| {
        // A theme's icon until the thumbnail comes, in the space it will take, so no row moves
        // under the pointer when it does.
        let image = gtk::Image::from_icon_name(Some("image-x-generic"), gtk::IconSize::Dialog);
        image.set_size_request(
            i32::from(control::MAX_THUMBNAIL_WIDTH),
            i32::from(control::MAX_THUMBNAIL_HEIGHT),
        );
        line.pack_start(&image, false, false, 0);
        Shown { image, title: title.clone() }
    });
    line.pack_start(&title, true, true, 0);
    line.pack_end(&detail, false, false, 0);
    widget.add(&line);
    (widget, shown)
}

fn set_title(label: &gtk::Label, row: &Row) {
    let escaped = glib::markup_escape_text(&row.title);
    label.set_markup(&if row.current { format!("<b>{escaped}</b>") } else { escaped.to_string() });
}

/// The thumbnail's pixels as a pixbuf. Only wrapped, never decoded: gdk-pixbuf's loaders never
/// see clipboard bytes (docs/ui.md, "Thumbnails").
fn pixbuf(t: &Thumbnail) -> Option<gtk::gdk_pixbuf::Pixbuf> {
    let (width, height) = (i32::from(t.width()), i32::from(t.height()));
    let stride = width.checked_mul(4)?;
    Some(gtk::gdk_pixbuf::Pixbuf::from_bytes(
        &glib::Bytes::from(t.rgba()),
        gtk::gdk_pixbuf::Colorspace::Rgb,
        true,
        8,
        width,
        height,
        stride,
    ))
}

/// Takes the keyboard and pointer for `window`, trying again for up to `within`.
fn grab(window: &gtk::Window, within: Duration) {
    let Some(w) = window.window() else { return };
    let Some(seat) = w.display().default_seat() else { return };
    let try_grab = move || {
        seat.grab(&w, gdk::SeatCapabilities::ALL, true, None, None, None)
            == gdk::GrabStatus::Success
    };
    if try_grab() {
        return;
    }
    let mut left = within.as_millis() / GRAB_EVERY.as_millis().max(1);
    glib::timeout_add_local(GRAB_EVERY, move || {
        left = left.saturating_sub(1);
        if try_grab() || left == 0 { glib::ControlFlow::Break } else { glib::ControlFlow::Continue }
    });
}
