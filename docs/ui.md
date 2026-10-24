# The desktop side (M2)

`jackstay-ui` (`crates/ui`), in Rust with gtk3-rs (plan §4.3). It holds no history: everything it
shows comes from `jackstayd` over the control socket, and everything it changes goes back the same
way. The popup and the tray icon are done; the settings window follows.

## The popup

`jackstay popup` shows the history at the pointer. Bound to a hotkey, it is clipman's
`xfce4-popup-clipman`.

- A popup window (override-redirect) of about 520 pixels wide, kept within the pointer's monitor's
  work area. It lists the newest 100 entries, newest first; the one on the clipboard is bold. A row
  is the start of the entry's text on one line (60 characters), or what it holds when it has no
  text (Image, Files, Rich text), with its size, "pinned" and where it was copied from.
- The window manager gives a popup window no focus, so it grabs the keyboard and pointer itself.
  A hotkey's own grab can still be held for a moment after it fires, so a refused grab is tried
  again every 25 ms for a second.
- Typing filters: every word, in any case, anywhere in the preview or the source. Only the
  preview is searched (the daemon sends the first 200 characters), not text past it.
- Keys: Up, Down, Page Up and Page Down move; Enter puts the selected entry on the clipboard and
  closes; Ctrl+P pins or unpins; Ctrl+D deletes; Esc closes. A click on a row puts it on the
  clipboard; a click outside closes.
- **Clear clipboard** (Alt+C) clears at once and closes. **Clear history** (Alt+H) asks first: the
  button becomes "Clear N entries? Press again", and a second press clears every unpinned entry
  and the clipboard. Closing forgets the first press. GTK does not activate button mnemonics in a
  popup window, so Alt+C and Alt+H are handled as keys.
- Nothing is filtered or hidden (plan §3): a copied password shows like anything else, and
  clearing is how to get rid of it.
- If the daemon cannot be reached, the popup opens anyway, empty, saying why.

## The tray

A StatusNotifierItem, which Xfce 4.20's systray hosts beside the legacy XEmbed icons (clipman's
among them). A click on the icon opens the history popup at the pointer, as clipman's icon opens
its history. The menu, which the panel draws and opens, has **Show history**, **Clear
clipboard**, and **Clear history…**, which asks first in a dialog whose default is Cancel (the
popup's second press cannot work from a menu).

- Exported through GLib's own D-Bus (gio), which gtk-rs links already: the
  `org.kde.StatusNotifierItem` interface at `/StatusNotifierItem` and `com.canonical.dbusmenu` at
  `/MenuBar`, under the name `org.kde.StatusNotifierItem-<pid>-1`. The plan named the `ksni`
  crate; it would have added 107 crates (zbus and an async runtime) for two small interfaces.
  gtk-rs 0.19 does not bind `GtkStatusIcon`, so clipman's XEmbed icon is not an option without
  `unsafe`.
- The item registers with `org.kde.StatusNotifierWatcher` only once its name is owned (a host
  quick to look would otherwise find nothing), and again whenever a watcher appears: a panel
  restarted, or started after the UI.
- The icon is `jackstay` (data/icons), installed by `tools/install` into
  `~/.local/share/icons/hicolor/scalable/apps` and named to the panel through `IconThemePath`.
- Without a session bus there is no tray, and the journal says so; the popup still works from
  its hotkey.
- A reply of the wrong D-Bus type is dropped by GLib, and the host shows no menu: the menu's
  replies are built as tuples of their members, and `tests/tray_menu.rs` checks every type.

## Threads

The GTK thread never touches a socket. A worker thread owns the connection to `jackstayd`
(reconnecting once if the daemon restarted, a 3 s limit on each exchange), and another serves the
UI's socket. Both hand their results to the GTK thread through `MainContext::invoke`; glib 0.22
has no Unix file-descriptor sources to watch the sockets from the main loop. A stalled daemon
leaves the popup waiting for its list, never frozen.

## The UI's socket

`$XDG_RUNTIME_DIR/jackstay/ui`, beside the control socket, in the same 0700 directory. `jackstay
popup` connects, writes `popup\n`, and reads `ok\n` once the UI has taken the request. The UI
answers only its own user (`SO_PEERCRED`), and gives a client 500 ms to say what it wants. One
UI runs at a time: it holds `ui.lock` in the runtime directory, and under that lock removes a
socket a previous UI left.

`jackstay popup` exits 0 once asked, and 3 if no UI answers. Run from a hotkey nothing shows its
stderr, so it also says so as a desktop notification (`notify-send`, when there is a display).

## Starting and stopping

`jackstay-ui.service`, a systemd user unit beside `jackstayd.service`, started by
`jackstay-session` from XDG autostart. It restarts on failure, except for statuses 0, 2 (bad
setup) and 3 (another UI running); 5 (no display yet) is retried. No core dumps: they would hold
the previews it shows. `tools/install` installs both; `--hotkey KEYS` binds the popup in Xfce's
keyboard shortcuts (`/commands/custom/KEYS` in the `xfce4-keyboard-shortcuts` channel), and
`--uninstall` removes only that binding.

## Tests

`crates/ui/tests/tray.rs` runs the UI on a private session bus with a stand-in watcher, and plays
the panel with `gdbus`: the item's registration and properties, the menu's layout, a click on the
icon, each menu item (the dialog's Cancel default included), a host's `EventGroup`, a panel
restarting, and no session bus at all. The tests give every UI `DBUS_SESSION_BUS_ADDRESS`
(`disabled:` when they want no bus): unset, GLib would try to start one.

`crates/ui/tests/popup.rs` runs the real `jackstay-ui` on Xvfb, driven with xdotool (keys go
through its grab, as a person's do), against a stand-in daemon that records every request: Enter,
moving, searching, Ctrl+D and Ctrl+P keeping the popup open, Alt+C, Alt+H's two presses, a click
outside, no daemon, and a second UI refused. `crates/ui/tests/model.rs` covers rows, search and
moving without a display.
