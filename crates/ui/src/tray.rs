//! The tray icon (docs/ui.md, "The tray"): a `StatusNotifierItem` with a `dbusmenu` menu, exported
//! on the session bus through `GLib`'s own D-Bus (`gio`), which gtk-rs already links. Xfce's systray
//! hosts it. A click on the icon opens the history popup at the pointer, as clipman's does; the
//! menu, which the panel draws, has the rest.

use std::collections::HashMap;
use std::rc::Rc;

use gtk::gio;
use gtk::glib::{self, Variant, prelude::ToVariant};

/// The item's object path, which the `StatusNotifierItem` spec fixes.
pub const ITEM_PATH: &str = "/StatusNotifierItem";
/// The menu's object path, named by the item's `Menu` property.
pub const MENU_PATH: &str = "/MenuBar";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
/// The icon's name, installed by `tools/install` into `~/.local/share/icons/hicolor`.
pub const ICON: &str = "jackstay";

const ITEM_XML: &str = r#"<node>
  <interface name="org.kde.StatusNotifierItem">
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="WindowId" type="i" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="IconThemePath" type="s" access="read"/>
    <property name="IconPixmap" type="a(iiay)" access="read"/>
    <property name="OverlayIconName" type="s" access="read"/>
    <property name="AttentionIconName" type="s" access="read"/>
    <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
    <property name="Menu" type="o" access="read"/>
    <method name="ContextMenu"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
    <method name="Activate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
    <method name="SecondaryActivate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
    <method name="Scroll"><arg name="delta" type="i" direction="in"/><arg name="orientation" type="s" direction="in"/></method>
    <signal name="NewIcon"/>
    <signal name="NewToolTip"/>
    <signal name="NewStatus"><arg name="status" type="s"/></signal>
  </interface>
</node>"#;

const MENU_XML: &str = r#"<node>
  <interface name="com.canonical.dbusmenu">
    <property name="Version" type="u" access="read"/>
    <property name="TextDirection" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconThemePath" type="as" access="read"/>
    <method name="GetLayout">
      <arg name="parentId" type="i" direction="in"/>
      <arg name="recursionDepth" type="i" direction="in"/>
      <arg name="propertyNames" type="as" direction="in"/>
      <arg name="revision" type="u" direction="out"/>
      <arg name="layout" type="(ia{sv}av)" direction="out"/>
    </method>
    <method name="GetGroupProperties">
      <arg name="ids" type="ai" direction="in"/>
      <arg name="propertyNames" type="as" direction="in"/>
      <arg name="properties" type="a(ia{sv})" direction="out"/>
    </method>
    <method name="GetProperty">
      <arg name="id" type="i" direction="in"/>
      <arg name="name" type="s" direction="in"/>
      <arg name="value" type="v" direction="out"/>
    </method>
    <method name="Event">
      <arg name="id" type="i" direction="in"/>
      <arg name="eventId" type="s" direction="in"/>
      <arg name="data" type="v" direction="in"/>
      <arg name="timestamp" type="u" direction="in"/>
    </method>
    <method name="EventGroup">
      <arg name="events" type="a(isvu)" direction="in"/>
      <arg name="idErrors" type="ai" direction="out"/>
    </method>
    <method name="AboutToShow">
      <arg name="id" type="i" direction="in"/>
      <arg name="needUpdate" type="b" direction="out"/>
    </method>
    <method name="AboutToShowGroup">
      <arg name="ids" type="ai" direction="in"/>
      <arg name="updatesNeeded" type="ai" direction="out"/>
      <arg name="idErrors" type="ai" direction="out"/>
    </method>
    <signal name="ItemsPropertiesUpdated">
      <arg name="updatedProps" type="a(ia{sv})"/>
      <arg name="removedProps" type="a(ias)"/>
    </signal>
    <signal name="LayoutUpdated"><arg name="revision" type="u"/><arg name="parent" type="i"/></signal>
    <signal name="ItemActivationRequested"><arg name="id" type="i"/><arg name="timestamp" type="u"/></signal>
  </interface>
</node>"#;

/// What the tray asks the UI to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ShowHistory,
    ClearClipboard,
    /// Asks first.
    ClearHistory,
    Settings,
}

/// The menu: (id, label, action); id 0 is the root, and a `None` label is a separator.
const MENU: [(i32, Option<&str>, Option<Action>); 6] = [
    (1, Some("Show history"), Some(Action::ShowHistory)),
    (2, None, None),
    (3, Some("Clear clipboard"), Some(Action::ClearClipboard)),
    (4, Some("Clear history…"), Some(Action::ClearHistory)),
    (5, None, None),
    (6, Some("Settings…"), Some(Action::Settings)),
];

/// The action of menu item `id`, if it has one.
#[must_use]
pub fn action(id: i32) -> Option<Action> {
    MENU.iter().find(|(i, ..)| *i == id).and_then(|(_, _, a)| *a)
}

fn properties(id: i32) -> HashMap<String, Variant> {
    let mut p = HashMap::new();
    match MENU.iter().find(|(i, ..)| *i == id) {
        Some((_, Some(label), _)) => {
            p.insert("label".to_owned(), label.to_variant());
        }
        Some((_, None, _)) => {
            p.insert("type".to_owned(), "separator".to_variant());
        }
        None if id == 0 => {
            p.insert("children-display".to_owned(), "submenu".to_variant());
        }
        None => {}
    }
    p
}

/// The menu's layout from `parent`, as `GetLayout` answers it: `(revision, (ia{sv}av))`.
#[must_use]
pub fn layout(parent: i32) -> Variant {
    let item = |id: i32, children: Vec<Variant>| (id, properties(id), children).to_variant();
    let tree = if parent == 0 {
        // glib boxes each element of an `av` itself.
        item(0, MENU.iter().map(|(id, ..)| item(*id, vec![])).collect())
    } else {
        item(parent, vec![])
    };
    // Built as a tuple of its members: `(1u32, tree)` would box the layout as `v`.
    Variant::tuple_from_iter([1u32.to_variant(), tree])
}

/// The properties of `ids`, as `GetGroupProperties` answers them.
#[must_use]
pub fn group_properties(ids: &[i32]) -> Variant {
    let all: Vec<i32> =
        if ids.is_empty() { MENU.iter().map(|(id, ..)| *id).collect() } else { ids.to_vec() };
    let props: Vec<(i32, HashMap<String, Variant>)> =
        all.into_iter().map(|id| (id, properties(id))).collect();
    (props,).to_variant()
}

fn item_property(name: &str) -> Variant {
    let icons = icon_theme_path();
    match name {
        "Category" => "ApplicationStatus".to_variant(),
        "Id" => "jackstay".to_variant(),
        "Title" => "Jackstay".to_variant(),
        "Status" => "Active".to_variant(),
        "WindowId" => 0i32.to_variant(),
        "IconName" => ICON.to_variant(),
        "IconThemePath" => icons.to_variant(),
        "IconPixmap" => Vec::<(i32, i32, Vec<u8>)>::new().to_variant(),
        "ToolTip" => {
            let none: Vec<(i32, i32, Vec<u8>)> = Vec::new();
            (ICON, none, "Jackstay", "Clipboard history: click to show").to_variant()
        }
        "ItemIsMenu" => false.to_variant(),
        "Menu" => glib::variant::ObjectPath::try_from(MENU_PATH.to_owned())
            .map_or_else(|_| "/".to_variant(), |p| p.to_variant()),
        _ => "".to_variant(),
    }
}

/// `$XDG_DATA_HOME/icons` (else `~/.local/share/icons`), where `tools/install` puts the icon.
fn icon_theme_path() -> String {
    std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
        .map(|d| d.join("icons").to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Exports the item and its menu on the session bus and registers with the panel's watcher,
/// again whenever a watcher appears. `act` runs each action, on the GTK thread.
///
/// # Errors
/// There is no session bus, or the objects could not be exported.
pub fn start(act: impl Fn(Action) + 'static) -> Result<(), glib::Error> {
    let bus = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)?;
    let act = Rc::new(act);
    let item = gio::DBusNodeInfo::for_xml(ITEM_XML)?;
    let menu = gio::DBusNodeInfo::for_xml(MENU_XML)?;
    let (Some(item), Some(menu)) = (
        item.lookup_interface("org.kde.StatusNotifierItem"),
        menu.lookup_interface("com.canonical.dbusmenu"),
    ) else {
        return Err(glib::Error::new(gio::IOErrorEnum::Failed, "interface missing"));
    };
    let on_item = Rc::clone(&act);
    bus.register_object(ITEM_PATH, &item)
        .method_call(move |_, _, _, _, method, _, invocation| {
            if method == "Activate" {
                on_item(Action::ShowHistory);
            }
            invocation.return_value(None);
        })
        .property(|_, _, _, _, name| item_property(name))
        .build()?;
    let on_menu = Rc::clone(&act);
    bus.register_object(MENU_PATH, &menu)
        .method_call(move |_, _, _, _, method, args, invocation| {
            invocation.return_value(menu_call(method, &args, &*on_menu).as_ref());
        })
        .property(|_, _, _, _, name| match name {
            "Version" => 3u32.to_variant(),
            "TextDirection" => "ltr".to_variant(),
            "Status" => "normal".to_variant(),
            "IconThemePath" => Vec::<String>::new().to_variant(),
            _ => "".to_variant(),
        })
        .build()?;
    let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    // The watcher is told of the item only once the name is ours, or a panel quick to look would
    // find nothing there. Owned and watched for as long as the process lives: the ids are not
    // handles, and dropping them unowns and unwatches nothing.
    let _ = gio::bus_own_name_on_connection(
        &bus,
        &name,
        gio::BusNameOwnerFlags::NONE,
        |bus, name| {
            let name = name.to_owned();
            let _ = gio::bus_watch_name_on_connection(
                &bus,
                WATCHER,
                gio::BusNameWatcherFlags::NONE,
                move |bus, _, _| register(&bus, &name),
                |_, _| {},
            );
        },
        |_, _| {},
    );
    Ok(())
}

/// The answer to a dbusmenu call, running the action of a clicked item.
fn menu_call(method: &str, args: &Variant, act: &dyn Fn(Action)) -> Option<Variant> {
    match method {
        "GetLayout" => {
            let parent = args.try_child_value(0).and_then(|v| v.get::<i32>()).unwrap_or(0);
            Some(layout(parent))
        }
        "GetGroupProperties" => {
            let ids = args.try_child_value(0).and_then(|v| v.get::<Vec<i32>>()).unwrap_or_default();
            Some(group_properties(&ids))
        }
        "GetProperty" => {
            let id = args.try_child_value(0).and_then(|v| v.get::<i32>()).unwrap_or(-1);
            let name = args.try_child_value(1).and_then(|v| v.get::<String>()).unwrap_or_default();
            let value = properties(id).remove(&name).unwrap_or_else(|| "".to_variant());
            Some((value,).to_variant())
        }
        "Event" => {
            let id = args.try_child_value(0).and_then(|v| v.get::<i32>()).unwrap_or(-1);
            let event = args.try_child_value(1).and_then(|v| v.get::<String>()).unwrap_or_default();
            if event == "clicked"
                && let Some(a) = action(id)
            {
                act(a);
            }
            None
        }
        "EventGroup" => {
            let events = args.try_child_value(0);
            for i in 0..events.as_ref().map_or(0, Variant::n_children) {
                let one = events.as_ref().and_then(|e| e.try_child_value(i));
                let id =
                    one.as_ref().and_then(|e| e.try_child_value(0)).and_then(|v| v.get::<i32>());
                let event =
                    one.as_ref().and_then(|e| e.try_child_value(1)).and_then(|v| v.get::<String>());
                if event.as_deref() == Some("clicked")
                    && let Some(a) = id.and_then(action)
                {
                    act(a);
                }
            }
            Some((Vec::<i32>::new(),).to_variant())
        }
        "AboutToShow" => Some((false,).to_variant()),
        "AboutToShowGroup" => Some((Vec::<i32>::new(), Vec::<i32>::new()).to_variant()),
        _ => None,
    }
}

fn register(bus: &gio::DBusConnection, name: &str) {
    bus.call(
        Some(WATCHER),
        WATCHER_PATH,
        WATCHER,
        "RegisterStatusNotifierItem",
        Some(&(name,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        5_000,
        None::<&gio::Cancellable>,
        |_| {},
    );
}
