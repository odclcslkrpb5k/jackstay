<p align="center"><img src="data/icons/jackstay.svg" width="128" alt="Jackstay icon: a clipboard hanging from a line between two posts"></p>

# Jackstay

Keyboard, mouse, and clipboard sharing across machines, plus clipboard management — for Xubuntu / Xfce4 on X11.

Jackstay is intended to replace both [Synergy](https://symless.com/synergy) and
[xfce4-clipman-plugin](https://docs.xfce.org/panel-plugins/xfce4-clipman-plugin).

## Goals

- **Flawless clipboard sharing and management** — CLIPBOARD and PRIMARY selections, multiple targets/MIME types, history
- **Real-time input sharing** — low-latency keyboard and mouse forwarding between hosts
- **Stability** — no dropped connections, stuck keys, or lost clipboard contents

## Platform

- Xubuntu / Xfce4
- X11

## Status

Early development. Working now: the clipboard manager (`jackstayd`), its history popup and tray
icon (`jackstay-ui`) and the `jackstay` command: CLIPBOARD copies kept after their application
quits and restored after a restart, a searchable history at the pointer or from the tray,
clearing, pinning, and xfce4-clipman's history imported. Not yet: settings (M2), and sharing
between machines (M3 to M5). See [the plan](docs/plan.md), [the daemon](docs/daemon.md) and
[the desktop side](docs/ui.md).

## Install

Needs Docker (the build runs in the development container) and Xubuntu 26.04 or another system
whose glibc is at least the container's (2.43).

```sh
tools/install --replace-clipman --hotkey '<Primary><Alt>h' --start
```

- Builds `jackstayd`, `jackstay` and `jackstay-ui` and copies them, with `jackstay-session`, to
  `~/.local/bin`; their systemd user units to `~/.config/systemd/user`; an autostart entry to `~/.config/autostart`.
  Nothing outside your home directory is touched. Running it again upgrades, restarting a
  running daemon.
- `--replace-clipman` stops xfce4-clipman and turns off its autostart, as clipman's own Quit
  does. Two clipboard managers fight, so `jackstayd` refuses to run beside clipman. If clipman
  is in the panel as a plugin, remove it there too (Panel Preferences, Items).
- `--hotkey KEYS` binds the history popup to KEYS in Xfce's keyboard shortcuts (here Ctrl+Alt+H;
  leave it out to bind one yourself to `~/.local/bin/jackstay popup`).
- `--start` starts it now; otherwise it starts at the next login.

On its first start Jackstay imports clipman's saved history behind its own. clipman's files are
left in place until `jackstay import-clipman --remove`.

## Use

```sh
jackstay list            # the newest 20 entries; --all for every one
jackstay select ID       # put an entry back on the clipboard
jackstay pin ID          # keep it through clears and limits (unpin ID to undo)
jackstay delete ID
jackstay clear           # clear the clipboard and delete what was on it, unless pinned
jackstay clear-history   # delete every unpinned entry and clear the clipboard
jackstay status
jackstay popup           # the history at the pointer: type to search, Enter to paste
```

Nothing copied is filtered or hidden: a copied password is kept like anything else, and
`jackstay clear` is how to get rid of it. Settings are in `~/.config/jackstay/config.toml`
([keys](docs/daemon.md#configuration)); the journal is `journalctl --user -u jackstayd`.

## Uninstall

```sh
tools/install --uninstall
```

Stops `jackstayd` and removes what `tools/install` added. The history stays in
`~/.local/state/jackstay`; delete that directory to remove it. Running `xfce4-clipman` turns its
autostart back on.

## Development

The only requirement is Docker. `./dev <command>` runs a command in the pinned development
container (Ubuntu 26.04, Rust 1.99.1), for example `./dev cargo build`. `./check` is the one gate,
the same one CI runs; `./dev tools/prove-checks` shows each of its checks catching a planted bug.

## License

[GNU General Public License v3.0](LICENSE).
