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

Early development.

## Development

The only requirement is Docker. `./dev <command>` runs a command in the pinned development
container (Ubuntu 26.04, Rust 1.99.1), for example `./dev cargo build`.

## License

[GNU General Public License v3.0](LICENSE).
