# Jackstay plan

Status: **draft** — nothing here is decided until it has been red-teamed and marked so.

## 1. What Jackstay is

One program that replaces two:

- **Synergy**: share one keyboard and mouse across several machines on a LAN, moving the pointer
  off a screen edge onto the next machine, with the clipboard following.
- **xfce4-clipman-plugin**: clipboard history and management on the local desktop.

Platform: Xubuntu, Xfce4, X11. Nothing else is in scope.

## 2. Optimization targets

In priority order:

1. **Flawless clipboard sharing and management.** No lost, truncated or stale clipboard contents,
   across machines or locally.
2. **Real-time input sharing.** Keyboard and mouse forwarding with latency and jitter the user
   cannot notice.
3. **Stability.** No crashes, dropped links, stuck keys or stuck modifiers.

## 3. Requirements

To be expanded and red-teamed. First cut:

### Clipboard
- Both X selections, CLIPBOARD and PRIMARY, and every target/MIME type the owner offers (text,
  rich text, images, URI lists), not only `UTF8_STRING`.
- Large contents through the INCR protocol, in both directions.
- Taking selection ownership back when the owning application exits (clipman's main job),
  without breaking applications that set the clipboard and stay running.
- History: browse, search, re-select, pin, delete.
- **No filtering of contents** (owner's decision, 2026-10-23). Everything copied is kept, whatever
  its source. Instead the user can **clear**: the current clipboard, one entry, or the whole
  history, at once, from the popup and the tray (clipman has this; it is a core requirement).
- Remote clipboard: the content follows the user to the machine they move to, and is offered
  there under the same targets.

### Known failures to beat

The owner's reported failures with Synergy and clipman (2026-10-23), with the mechanism each most
likely comes from. Each is to be reproduced as a conformance case (§5.7) before it is called fixed.

1. **Images fail to copy from some applications.** Most likely target negotiation. Applications
   offer different image targets (`image/png`, `image/bmp`, `image/jpeg`, `image/x-*`, or only
   their own), and a tool that knows one target misses the others. Jackstay keeps whatever image
   target the owner offers, and **offers images under every common image target**, transcoding
   from what it holds (pure-Rust decoders, in the daemon), so any requestor finds a target it
   accepts.
2. **Images fail to copy from every application.** Most likely size. A screenshot exceeds the
   maximum request size, so it needs INCR, or it exceeds a tool's own clipboard size cap. Jackstay
   supports INCR both ways, and has limits sized for real screenshots (a 4K PNG is several MB, and
   raw RGBA is about 33 MB).
3. **Nothing copies from any application.** Most likely a wedged state. One stalled transfer, or a
   lost ownership race, leaves a single-threaded clipboard path stuck until restart. Jackstay
   gives each transfer its own state machine and timeout (§5.2). A self-check periodically
   confirms that the CLIPBOARD owner answers TARGETS, and recovers if it does not, rather than
   waiting for a restart.

### Input
- Capture keyboard and mouse on the machine with the physical devices; inject on the others (XTest).
- Screen-edge switching with a configurable layout; hotkeys to switch and to lock.
- Correct key mapping between machines with different keymaps; no stuck keys or modifiers when a
  link drops or the pointer switches mid-press.

### Network
- Encrypted and mutually authenticated link between machines (Synergy's weakest point).
- Survives sleep, Wi-Fi drops and restarts on either side; reconnects without user action.

### Desktop
- Xfce panel presence (status, history menu), a settings window, autostart.

## 4. Tech stack

**Decided 2026-10-23: Rust** (owner's approval), as recommended in §4.3, with gtk3-rs for the UI
(the recommended default; the owner did not choose otherwise). The house standard, [Paranoid
TypeScript](https://github.com/odclcslkrpb5k/paranoid-typescript), has an appendix for C + GTK 3;
using that stack must be justified, and alternatives are welcome if they serve the targets better.

Criteria, in order: how well the stack serves each optimization target (§2); how much of the
standard's control apparatus it lets us push up the ladder (Eliminate/Substitute rather than
Engineer/Administer); maturity of its X11 (XFixes, XInput2, XTest, selections/INCR) and Xfce panel
integration; and how much of the standard already carries over to it.

### 4.1 What the problem demands of a stack

Three facts about the problem decide most of the choice.

1. **"Flawless clipboard" means speaking the selection protocol directly.** The ICCCM selection
   protocol is where clipboard tools lose data: TARGETS, MULTIPLE, INCR in both directions,
   TIMESTAMP, the CLIPBOARD_MANAGER / SAVE_TARGETS handover, and XFixes selection-change events.
   Toolkit clipboard APIs (GTK's `GtkClipboard`, which xfce4-clipman uses) hide these and decide
   for us which targets survive. So the clipboard core talks raw X11 whatever the UI toolkit is;
   the toolkit choice is a UI decision, not a clipboard one.
2. **Everything Jackstay parses is hostile.** Clipboard contents come from every application on
   the desktop and, once shared, from other machines over the network. Memory-unsafe parsing of
   network input is the worst defect class a program like this can have. This is not
   hypothetical: Deskflow (the Synergy fork, C++) had
   [CVE-2026-41476](https://bugzilla.redhat.com/show_bug.cgi?id=2461689), an out-of-bounds read
   from a malformed clipboard update. The read is in `IClipboard::unmarshall()`; the size check in
`ClipboardChunk::assemble()` validated only the outer transfer size, so malformed inner lengths
got through. The lesson: every inner length is checked against the bytes remaining, not the
outer total.
3. **Real-time input must not share a thread with a UI.** Pointer motion has to be forwarded in
   well under a frame. A GUI main loop (GTK's) stalls on redraws, D-Bus calls and handlers; the
   C + GTK appendix itself needs a "stall watch" to catch callbacks that block it. The input and
   clipboard path should run in its own process with no toolkit, so a UI hang or crash cannot
   freeze input or lose the clipboard.

### 4.2 Candidates

| | C + GTK 3 | Rust + x11rb (+ gtk3-rs UI) | C++ / fork Deskflow | Go |
|---|---|---|---|---|
| Memory safety | hand-built: poison header, taught `-fanalyzer`, sanitizers, fuzzing | by construction outside `unsafe`; panics still possible (§4.3) | none; CVE above | yes (GC) |
| Ladder fit with the standard | appendix exists, proven on one project | `Result`, `#[must_use]`, exhaustive `match`, newtypes are language features — the standard cites them as its prior art | weak | no exhaustiveness, nil, errors ignorable |
| X11 access | xcb / Xlib, full | x11rb: full XCB protocol incl. XFixes, XInput2, XTest | Deskflow's own X11 layer | jezek/xgb, thinner |
| Latency | no GC | no GC | no GC | GC pauses (small, nonzero) |
| Xfce-native UI | GTK 3, native | gtk3-rs (GTK 3, native) | Qt, foreign look | gotk3 (GTK 3) |
| Mutation / fuzz / property tests | by hand; libFuzzer; Mull unpackaged | cargo-mutants, cargo-fuzz, proptest | as C | built-in fuzzing |
| Cost | largest control apparatus to build | no appendix yet; compile times; crate supply chain | inherits cross-platform baggage, Qt, protocol | weakest ladder |

Rejected without a column:

- **GTK 4** (any language): its X11 backend is
  [deprecated since 4.17.4](https://www.phoronix.com/news/GTK-X11-Now-Deprecated), for removal in
  GTK 5, and Xfce 4.20's themes and panel are GTK 3. Building an X11-only app on a toolkit
  retiring X11 is backwards.
- **Fork lan-mouse** (Rust, maintained): its README says X11 supports only input emulation, not
  capture — the half we need most — and it is Wayland-first and cross-platform. Useful as prior
  art for protocol and capture design, not as a base.
- **Input Leap**: archived. **Zig**: pre-1.0. **Python**: not for the latency path.

### 4.3 Recommendation: Rust, two processes

**Revised after red-team pass 1 (§7).**

#### `jackstayd` — Rust, no toolkit

Owns everything the optimization targets depend on. Three threads of work, each with its own
X11 connection where it needs one, joined by channels:

- **Clipboard** — its own X connection via [x11rb](https://github.com/psychon/x11rb)'s pure-Rust
  `RustConnection`: selections (CLIPBOARD, PRIMARY), TARGETS, MULTIPLE, INCR, XFixes
  selection-change events, CLIPBOARD_MANAGER/SAVE_TARGETS, and the history store. This is where
  hostile data enters from local applications, and it has **no C code in its path** and
  `#![forbid(unsafe_code)]`. Each concurrent INCR transfer is its own state machine with a
  timeout, so a slow requestor stalls only itself.
- **Input** — a separate X connection, so an INCR burst or a 16 MB selection reply never queues in
  front of pointer motion. XInput2 events, XFixes pointer barriers with XI 2.3 barrier events for
  edge detection, grabs while the pointer is on another machine, and XTest injection. Keymaps need
  [libxkbcommon-x11](https://xkbcommon.org/), the library that compiles and tracks XKB keymaps.
  Its API takes a libxcb `xcb_connection_t*`, so this connection is x11rb's `XCBConnection`, which
  means libxcb and libxkbcommon (C) are in this path. That is a **Substitute** choice: libxkbcommon
  is used by every Wayland compositor and is heavily exercised, and hand-writing XKB keymap
  handling is where Synergy-likes get stuck keys. The `unsafe` is confined to one adapter crate
  (ER-3); data reaching it comes from the X server and from fixed-size input messages, not from
  clipboard contents. **Superseded (M4 design, 2026-10-29):** no keyboard state machine is needed, so there is no
  libxkbcommon, no libxcb and no `unsafe`; the input connection is a `RustConnection` too
  (docs/input.md, "No C after all").
- **Network** — [quinn](https://github.com/quinn-rs/quinn) (QUIC over rustls, on tokio): one
  mutually authenticated session per peer, with pinned certificates. Input travels on its own
  stream (or datagrams for pointer motion), and each clipboard transfer on its own stream, so a
  large transfer cannot delay a keypress (no head-of-line blocking). One handshake, one auth path
  and one reconnect path, where two TCP+TLS links would need two of each.

The X threads run blocking poll loops (no async runtime in the X path); tokio is confined to the
network thread.

#### `jackstay-ui` — Rust + gtk3-rs

History popup, settings window and tray icon. Talks to the daemon over a Unix socket in
`$XDG_RUNTIME_DIR` (directory 0700, peer checked with `SO_PEERCRED`). The protocol's types live in
one crate that both processes use (RP-5, TS-7), so the two sides cannot drift.

- **History popup:** an override-redirect GTK window owned by the UI, opened at the pointer by
  `jackstay popup`. Its hotkey is registered in Xfce's `xfce4-keyboard-shortcuts` xfconf channel
  (as `xfce4-popup-clipman` is), not with `XGrabKey`, which would collide with xfsettingsd's grabs.
- **Tray:** a StatusNotifierItem. Planned through [ksni](https://crates.io/crates/ksni) (pure
  Rust, zbus); built instead on gio's D-Bus, which gtk-rs links already, since ksni would have
  added 107 crates for two small interfaces (docs/ui.md, "The tray", 2026-10-24). gtk3-rs has no
  SNI support. Xfce 4.20's systray hosts SNI items with dbusmenu menus, but the
  panel owns that menu and cannot open it at the pointer, so it is a secondary entry point only.
- **Images:** the daemon decodes clipboard images with pure-Rust decoders and hands the UI raw
  RGBA thumbnails with size limits. gdk-pixbuf (C) never decodes untrusted bytes.

**Why gtk3-rs for the UI, and not C + GTK 3 under the existing appendix:** the UI is isolated, so
either works, and C + GTK 3 is the proven option. Rust wins on one point that is Engineer rather
than Administer: both sides of the socket protocol come from the same type definitions. It also
means one language, one toolchain and one set of lint and test gates. The risk: gtk3-rs is a
**single-maintainer revival** (archived 2024; 0.19.0 released 2026-09-08 after a gap since 2023;
98 of the last 100 commits by one person). We pin it, and if it dies the UI can be rewritten in
C + GTK 3 behind the same socket protocol without touching the daemon. This is the owner's call
(§4.4).

Rejected for the UI: egui, iced and slint, which avoid GTK's FFI but look foreign on Xfce and
bring no tray; and a C panel plugin, which would put a crashable process inside the panel and
duplicate what SNI plus a popup already gives.

#### Why on the ladder

Safe Rust turns memory-safety defects into compile errors or bounds-checked panics (Eliminate for
corruption and information disclosure), outside `unsafe`, which our crates forbid except the
keymap adapter. Chapter 04's `Result`/`panic` model, and chapter 03's exhaustive switches and used
results, are compiler-enforced rather than lint-enforced.

That is **not** the end of the parsing problem. CVE-2026-41476 in safe Rust would be a panic. With
`panic = "abort"` that kills the daemon, dropping the clipboard and the input link: a remotely
triggerable denial of service against targets 1 and 3. With `panic = "unwind"` (the tentative
choice, §5.8), a panic caught at a task boundary costs one connection or transfer. But only where
something catches it, and only if no shared state it touched is left poisoned. Either way, the
parsers must not panic in the first place. Engineer controls apply to every decoder (wire
messages, selection data, images):

- clippy's `indexing_slicing`, `arithmetic_side_effects` and `unwrap_used` denied in parser
  modules, so a parser cannot panic on input;
- cargo-fuzz on every decoder, with its seed corpus run as ordinary tests (TS-16);
- size limits enforced before allocation;
- Protect: a systemd user unit restarts the daemon. On restart it claims the clipboard only if it
  has no owner (§5.2), and peers release held keys when the heartbeat stops (§5.3).

**Costs we accept, and their controls:**

- **No Rust appendix exists.** We would propose one to the standard's owning session, built from
  what this project proves, as the C appendix was. Until then the core applies as written where it
  can, and each mapping we use is recorded in `CLAUDE.md`.
- **Crate supply chain** (TC-3, TC-4, TC-7): `Cargo.lock` committed, `--locked` builds,
  `cargo-deny` for advisories, licenses and sources, a minimal dependency list with each crate
  justified. quinn and tokio are the largest; they buy the transport above.
- ~~**C in the input path** (libxcb, libxkbcommon), in one adapter crate, as above.~~ Removed by
  the M4 design (docs/input.md).
- **A second local protocol** (the UI socket), which is a parser too: fuzzed like the others, and
  versioned from its first message.

#### Hard problems this stack must handle

These shape the design (§5) more than the stack, but the stack has to reach them:

- **Stuck keys.** Measured (corrected 2026-10-29, §5.3): the X server does **not** release
  XTest-held keys when the injecting client disconnects, so the secondary clears the XTest
  devices' held keys itself at start and on every entry, and the held-key heartbeat covers a
  dropped link.
- **Grab failure.** Active grabs fail with `AlreadyGrabbed` while a menu is open or the screensaver
  holds a grab. The pointer needs a defined behaviour when it cannot leave.
- **Remote ownership.** While the user works on another machine, the local daemon must own the
  selection with a correct timestamp, taken from a zero-length property append, since there is no
  user event to take one from. Targets are offered at once and fetched lazily over the network, so
  large items need a latency policy.
- **Coexistence.** xfce4-clipman must not run beside Jackstay, or selection ownership flaps;
  nor may xfsettingsd's own clipboard manager, which this plan first said Xfce 4.20 lacked (it
  has one, off only with `XFSETTINGSD_NO_CLIPBOARD`: docs/daemon.md, "Coexistence"). Synergy and Deskflow conflict only with input
  sharing. See §5.6.

### 4.4 Decisions

- **Rust**, approved by the owner 2026-10-23.
- **UI: gtk3-rs**, the recommended default. Fallback: C + GTK 3 behind the same socket protocol.
- A Rust appendix is proposed to the standard's owning session; until it exists, mappings are
  recorded in `CLAUDE.md`.

## 5. Architecture

**Revised after red-team pass 2 (§7). Not yet red-teamed again after revision.**

### 5.1 Workspace

One Cargo workspace. Each crate has one job, and none contains `unsafe` (`x11-input` was to, for
libxkbcommon, until docs/input.md removed the need).

| Crate | Job | `unsafe` |
|---|---|---|
| `proto` | Wire and socket message types, their encoders and decoders, version negotiation | forbidden |
| `clip` | The clipboard model and history store; no I/O | forbidden |
| `selection` | The ICCCM selection protocol as pure state machines: capture, INCR both ways, MULTIPLE, keep-alive, CLIPBOARD_MANAGER; talks to X only through a port trait | forbidden |
| `x11-clip` | Implements that port over x11rb's `RustConnection`; the clipboard thread's event loop | forbidden |
| `x11-input` | Barriers, grabs, XI2, XTest, XKB locks, over x11rb's pure-Rust connection (docs/input.md: no libxkbcommon after all) | forbidden |
| `net` | QUIC sessions, pairing, reconnect | forbidden |
| `jackstayd` | Wires the above into threads and channels; config; supervision | forbidden |
| `jackstay-ui` | gtk3-rs popup, settings, ksni tray | forbidden in our code |
| `jackstay` | CLI: `popup`, `clear`, `switch <screen>`, `lock`, `status`, `pair`; a thin socket client | forbidden |

Logic lives in `clip`, `selection` and `proto`, which need no display server and are tested by
scripted event sequences, property tests and fuzzing (`selection` and `proto` read bytes other
programs control, so they deny every panicking construct, as `proto` does). The X11 crates are thin and are tested against real X servers (§5.7).

### 5.2 Clipboard model

An **entry** is one snapshot of one selection: the targets the owner offered, the bytes of the
targets we kept, the source (the owner window's `WM_CLASS`, or a peer), and the X timestamp of the
change. A copy is an entry, not a string.

**Capture (CLIPBOARD).** On an XFixes `SelectionNotify`, fetch TARGETS, then fetch the
**canonical** target of each class the owner offers: `UTF8_STRING`; `text/html`; `image/png`
(else the first image type); `text/uri-list`; `x-special/gnome-copied-files`. Use
`ConvertSelection` with the event's timestamp, never `CurrentTime`. Fetching every target would
force lazy renderers (LibreOffice, GIMP) to produce every format on every copy. Other targets stay
available while the original owner lives, because the owner serves them. After keep-alive we offer
only what we hold, so we never offer a target we cannot serve. INCR is supported, through one state
machine per transfer, with a timeout. A target over the size limits is recorded as not kept.

**Capture (PRIMARY).** Off by default in history. When on, capture only after Button1 and Shift
are released, as clipman does (`cb_check_primary_clipboard` in its `collector.c`). Ownership is
set when the drag starts and the text grows without a new event. PRIMARY is never kept alive by
default.

**Keep-alive (CLIPBOARD).** When XFixes reports `SelectionClientClose` or `SelectionWindowDestroy`,
the owner is already gone. `jackstayd` claims ownership with that event's timestamp, which can
never overwrite a newer owner, and serves the entry it captured at copy time. There is a short
window in which a paste finds no owner. It is unavoidable without CLIPBOARD_MANAGER (below) and is
as wide as our event latency. While an owner lives, we never take ownership, except in the
handoff below and at re-select.

**CLIPBOARD_MANAGER handoff** (CLIPBOARD only). `jackstayd` owns the `CLIPBOARD_MANAGER`
selection and announces it with the ICCCM `MANAGER` client message. When an exiting application
(GTK does this) asks it to convert `SAVE_TARGETS`, it reads the target list from the request's
property, fetches those targets through `MULTIPLE` while the application is still alive, then
takes CLIPBOARD ownership and replies. This closes the keep-alive gap for applications that use it.

**Serving.** TARGETS, MULTIPLE, TIMESTAMP, and INCR for any reply larger than the maximum request
size, with one state machine per requestor, so a slow or dead requestor stalls only itself.

**No filtering.** Jackstay never decides that content should not be kept: no password-hint
handling, no source exclusion lists. The owner knows what is in their clipboard and decides what to
remove (§3).

**Clearing.** Each of these is one action in the popup and the tray, and also `jackstay clear`:
- **Clear clipboard:** empties CLIPBOARD (and PRIMARY). If another application owns it, for
  example a password manager that is still running, `jackstayd` takes ownership with a fresh
  timestamp and offers no targets, so a paste yields nothing. The current entry is removed from
  history.
- **Delete entry:** removes one entry. If it is the current clipboard, the clipboard is cleared
  as above.
- **Clear history:** removes every unpinned entry, and also clears the clipboard. Pinned entries
  need their own delete, so a pin is never lost to a habitual clear.
Removal deletes the entry's files from disk, not just its index line. It also propagates to peers
that received the entry (§5.4), so a password cleared on one machine is not still pasteable on
another.

**History.** A bounded list with pin, delete, search and re-select (which makes `jackstayd` the
owner of that entry, as a user action with a fresh timestamp). Every entry is persisted, under
`$XDG_DATA_HOME/jackstay/` (directory 0700, files 0600). Blobs are content-addressed files beside a
small index that is rewritten atomically, so a copy of a 16 MB image does not rewrite the store.
The format is our own and versioned. Pure-Rust storage keeps the clipboard path free of C (no
SQLite). A store that fails to parse is moved aside and history starts empty: a corrupt file never
stops the clipboard. On first run, text history is imported from clipman
(`~/.cache/xfce4/clipman/textsrc`).

**Restart.** On start, `jackstayd` claims CLIPBOARD only if `GetSelectionOwner` returns None, and
serves the newest persisted entry.

### 5.3 Input model

One machine has the physical keyboard and mouse: the **primary**. The others are **secondaries**.
A layout maps screen edges to neighbours. It is recomputed on RandR changes (monitors added,
removed or moved), and edges between a machine's own monitors are never barriers.

- **Leaving.** XFixes pointer barriers sit on the edges that have a neighbour, with XI 2.3 barrier
  events for the crossing point and velocity. On a crossing, the primary grabs keyboard and pointer
  (`XIGrabDevice` on the master devices), hides the cursor, and forwards XI2 raw events to the
  active secondary.
- **No crossing mid-drag.** If a button is held, the pointer has an implicit grab, so ours would
  fail, and a cross-machine drag cannot work anyway. The barrier holds until the button is
  released.
- **Grab failure.** If the grab fails (`AlreadyGrabbed`: a menu is open, or the screen is locked),
  the pointer stays on the primary, and the crossing is retried on the next push against the
  barrier. It never half-switches.
- **Modifiers.** On every switch, in both directions, the side gaining focus is told the full
  modifier and lock state, and the side losing it releases everything it holds. A Shift held
  across a crossing is therefore pressed on the far side and released on the near side.
- **Keys.** Both ends are Xubuntu with evdev, so keys travel as keycodes plus the active XKB group
  and are injected exactly. Groups and `MappingNotify` changes are tracked through libxkbcommon
  (superseded: the core keymap and XKB's lock request suffice, docs/input.md).
  If a keycode's keysym differs on the secondary (detected per key, not by hashing whole keymaps,
  which differ across xkeyboard-config versions), the keysym is sent and resolved there, borrowing
  a spare keycode if needed. Borrowed keycodes are restored on exit and checked and restored at
  startup, so a crash cannot leave the keymap modified.
- **Auto-repeat.** The secondary repeats held keys itself, with its own repeat settings. The
  primary suppresses its repeats while grabbing, so the user never gets double repeats.
- **Held-key heartbeat.** While any key or button is held, stream 0 carries a heartbeat about
  every 100 ms listing what is held. If heartbeats stop for about 300 ms ("suspect"), the secondary
  releases everything. That is before Xfce's default 500 ms repeat delay, so a lost release cannot
  turn into an endless repeat. A link is "dead" only after several seconds without traffic, which
  tolerates Wi-Fi power-save pauses. A held key with no messages is normal, so silence alone never
  releases anything. Only a missing heartbeat does. **Measured (corrected 2026-10-29, Xvfb
  21.1.22):** XTest-held keys stay down after the injecting client's connection closes, whether it
  exits, aborts or is killed. The M0 spike had reported the opposite because its client was
  Xvfb's only one, and Xvfb resets, clearing every key, when its last client leaves. So a daemon
  crash *can* leave a key stuck on the secondary: the secondary releases whatever the XTest
  devices hold at start and on every entry (docs/input.md, "Secondary").
- **Returning.** The secondary reports the pointer reaching an edge, and the primary ungrabs and
  warps its pointer to the matching point.
- **Hotkeys** (switch, lock to screen) are seen by the primary while it is grabbing, and are
  `jackstay switch` and `jackstay lock` bound through xfconf when it is not.

### 5.4 Clipboard sharing

Clipboard is symmetric: any machine may copy and any may paste.

- When an entry is captured on machine A, A announces it to every peer: an id, its targets and
  sizes. Targets under 64 KiB travel with the announcement. Larger ones are fetched on demand.
- A peer takes CLIPBOARD ownership when Jackstay's own input sharing moves the pointer onto it.
  It offers exactly the targets A holds. Its timestamp comes from a zero-length property append.
  This depends on input sharing, so input sharing ships first (§6).
- A paste on B of a target B does not hold is answered only once B has fetched the **whole**
  target from A. B holds back the `SelectionNotify` until then, within a deadline below common
  requestor timeouts (about 3 s; Qt waits about 5 s), and then serves it locally, with INCR if it
  is large. If the fetch fails or misses the deadline, B refuses (property None) before any byte is
  sent. Streaming INCR straight from the network is ruled out: once INCR has started, the only way
  to end it is a zero-length chunk, which the requestor reads as "complete", so a dropped link
  would silently truncate the paste.
- Received entries join B's history with their source recorded. They are offered for paste and
  never re-announced. Clearing on any machine (§5.2) is announced too, and every peer removes the
  entry and drops its ownership of it.
- Synergy's own clipboard sharing must be off on every machine Jackstay shares with. Otherwise
  both programs claim ownership on every switch, and each captures the other's claim as a new
  entry. Because input sharing ships first, Synergy is gone by then.

### 5.5 Network and trust

- QUIC (quinn, rustls) on one UDP port. Each machine has a long-lived Ed25519 key, generated on
  first run and stored 0600. Authentication uses raw public keys (RFC 7250) if rustls supports
  them for both sides, and otherwise self-signed certificates pinned by key. Either way, the custom
  verifier still checks the handshake signature.
- **Pairing** is an explicit mode, entered with `jackstay pair` on both machines and lasting about
  2 minutes. Only in that mode, and only on a separate pairing endpoint, is a handshake with an
  unknown key accepted. Both machines show a short code derived from the TLS session's exporter
  and from nonces each side commits to before seeing the other's, so an attacker in the middle
  cannot grind a matching code, even offline as the TLS server (docs/link.md, "Pairing"). The user
  compares the codes and confirms on both sides. Outside pairing mode, an unknown key is refused in
  the handshake, before any message is parsed.
- Peers are addressed by configured hostnames. Discovery (mDNS) is not in the first version.
- One session per peer. Stream 0 carries control, input and pointer motion in order, so a click
  never overtakes the motion that put the pointer there (LAN loss is near zero, so head-of-line
  cost on stream 0 is negligible). Each clipboard transfer gets its own stream. Reconnect is
  automatic, with backoff.
- Every message is decoded by `proto` from bytes into a declared type (BD-1), with each inner
  length checked against the bytes remaining, before allocation (the lesson of CVE-2026-41476).

### 5.6 Configuration and operation

- `~/.config/jackstay/config.toml`, parsed once at start into a typed config. An invalid config
  refuses to start, with the field named. The settings window writes the same file.
- `jackstayd` is a systemd user unit, started from XDG autostart by a script that runs
  `systemctl --user import-environment DISPLAY XAUTHORITY` and then starts the unit, because Xfce
  never reaches `graphical-session.target`. `Restart=on-failure`, with the "X connection lost"
  exit code in `RestartPreventExitStatus`, so logging out does not cause a restart loop against a
  dead display. The UI restarts itself when the daemon restarts.
- Logs go to the journal and name kinds and codes, never clipboard contents or file paths (DP-3).
- **Coexistence.** The clipboard manager coexists with Synergy's input sharing. It refuses to start
  if another clipboard manager is detected:
  - by process name: `xfce4-clipman` (the standalone build, which the owner runs), CopyQ,
    Parcellite, Diodon, GPaste;
  - via the xfconf panel plugin list, for clipman's panel plugin, which runs inside the panel's
    `wrapper-2.0` process;
  - by behaviour: another client re-taking ownership right after each of our keep-alive claims.
  Input sharing refuses to start while Synergy or Deskflow is running.
- **Rollback.** Install never removes clipman or Synergy. It disables their autostart and the
  clipman panel plugin, and `jackstay uninstall-integration` restores them.

### 5.7 Testing strategy

Machine checks catch what they can. The rest is tested against real systems, not fakes the agent
wrote (the standard's "independent layers").

- **Unit and property tests** for `clip` and `proto`. **cargo-fuzz** on every decoder, including
  the history index. **cargo-mutants** on regression tests and checkers (TS-5).
- **ICCCM conformance matrix:** copy from and paste into real applications on Xvfb, for text,
  HTML, images, URI lists and files, with small, INCR-sized and limit-sized payloads, including the
  source exiting before the paste and the CLIPBOARD_MANAGER handoff. It is built in two tiers:
  - **Core tier (M1):** xclip, xsel, a GTK 3 and a GTK 4 test client, a Qt 6 test client, xterm.
    Each is cheap to drive.
  - **Application tier (M2):** Firefox (Mozilla's apt package or tarball, not the snap), Chrome for
    Testing (Ubuntu's Chromium is snap-only and snaps do not run in Docker), LibreOffice (driven
    through UNO), xfce4-terminal, and one named Electron app. Each application needs a driver
    (xdotool, Marionette, CDP, UNO), and that is sized as real work in M2.
  The owner's reported Synergy and clipman failures become cases in this matrix.
- **Two-machine tests:** two Xvfb servers and two daemons in one container, on loopback, with a
  UDP relay in the test process between them for delay, loss, reordering and black-holing (in
  place of `tc netem` between containers: no privileges, exact faults; docs/link.md, "Testing"). Inject input on one and assert what the other
  receives, including links dropped mid-keypress, heartbeat loss, and held modifiers across a
  crossing. **Measured (M0 spike, Xvfb 21.1.22, XFixes 6.0, XInput 2.4):** *relative* XTest motion
  is stopped by a pointer barrier and delivers `XI_BarrierHit` events. *Absolute* XTest motion
  passes straight through, like a warp. The harness therefore drives the pointer with relative
  motion, as a real mouse does. Absolute devices (tablets, touchscreens) on the primary would
  cross no barriers, which is acceptable for a first version.
- **Latency.** In the harness, both sides timestamp with `CLOCK_MONOTONIC`, which containers on one
  host share. X event timestamps are millisecond-grained and per-server, so they are useless for
  this. CI gates on a loose loopback budget, set from M0 measurements, that catches regressions
  without flaking. On the owner's LAN, latency is measured as halved round trips through an echo,
  and the budget there (initially p99 under 2 ms added) is an acceptance check for input sharing,
  not a CI gate.
- **Soak:** a 24-hour run with random copy, paste and input, with zero leaks, zero stuck keys and
  zero lost entries.

### 5.8 Toolchain

- Docker image pinned by digest, with apt from a dated snapshot (TC-9). Rust pinned in
  `rust-toolchain.toml`, plus one pinned nightly, used only for cargo-fuzz.
- `./check` is the one gate command (TC-5): `cargo fmt --check`, `clippy` with `-D warnings`
  (pedantic, plus the parser no-panic lints), `cargo deny`, tests, fuzz seed corpora, and the
  conformance matrix. CI runs exactly `./check`.
- `Cargo.lock` committed, builds `--locked`. Each dependency is justified in
  `docs/dependencies.md` (TC-7).
- `[workspace.lints]` carries `unsafe_code = "forbid"` to every crate.
  Release profile: `overflow-checks = true`. `panic = "unwind"` in `jackstayd`, so a panic in one
  connection's or transfer's handler can be caught at that task's boundary and costs that
  connection, not the daemon. To be measured in M0, not assumed, including: what a panic while
  holding a `std::sync::Mutex` does to the next locker (poisoning); whether tokio's `JoinHandle`
  really contains a task panic; and what happens to a panic on the blocking X poll threads, which
  no runtime catches (a supervisor thread that turns it into a clean daemon restart is the
  fallback).
- **For the Rust appendix**, the standard's session asked for these to be measured in M0, each
  with its exact setting, tool version, planted bug, command and exit status: which clippy
  restriction lints are livable in parsers (`indexing_slicing`, `arithmetic_side_effects` against
  checked or wrapping types, `unwrap_used`, `expect_used`, `panic`, `string_slice`); whether
  `[workspace.lints]` reaches every crate; `overflow-checks` and the choice of panic strategy;
  whether cargo-fuzz finds a planted out-of-bounds parse, and how fast; what cargo-mutants reports
  for a test cut off by its timeout; and whether `--locked` and cargo-deny catch planted lockfile
  drift and a planted advisory.

## 6. Milestones

Ordered so that the owner gets the highest-priority target into daily use first, each milestone is
usable on its own, and each target's main control exists before the owner depends on it.

| | Milestone | Done when |
|---|---|---|
| M0 | Skeleton and spikes | Workspace, Docker image, `./check`, CI, lint and deny configs, one planted bug per control watched failing; the standard session's measurement list (§5.8) answered; spikes: XTest motion triggers barriers on Xvfb, and whether XTest keys stay down after the injecting client dies (they do: corrected 2026-10-29). Spikes **done 2026-10-23** (`spikes/x11-probes`; results in §5.3 and §5.7) |
| M1 | Headless clipboard manager | Capture, keep-alive, CLIPBOARD_MANAGER, history store, clearing (`jackstay clear`), clipman import, `jackstay popup` placeholder; core tier of the conformance matrix in `./check`. **Done 2026-10-24** (docs/daemon.md, docs/conformance.md; `tools/install`) |
| M2 | Clipboard UI and application tier | Popup, tray, settings; application tier of the matrix (built 2026-10-25, with VS Code as the Electron application; docs/conformance.md). **Dogfood:** the owner disables clipman for two weeks; done when no lost or wrong entry is reported, and every report has become a matrix case |
| M3 | Link | Pairing, QUIC session, reconnect, heartbeat, two-machine harness (docs/link.md) |
| M4 | Input sharing | §5.3 working; latency acceptance on the owner's LAN. **Dogfood:** the owner runs Jackstay instead of Synergy for input (Synergy stays installed as the fallback), with Synergy's clipboard sharing missed for the duration |
| M5 | Clipboard sharing | §5.4 working; `proto` adopts `clip`'s lossless Latin-1 `TargetName` (up to 4096 bytes, u16 length on the wire) in place of its ASCII-only one, and the fuzz corpus is regrown (moved from M3: nothing sends an `Announce` before this); the owner uninstalls Synergy |
| M6 | Hardening and packaging | Soak test passing, fuzzing in CI, a `.deb` |

Input sharing comes before clipboard sharing because §5.4 takes ownership on Jackstay's own pointer
arrival, which needs Jackstay's input sharing. While Synergy still moves the pointer there is no
arrival event, and both programs would fight over ownership. The cost is that M4's dogfood has no
cross-machine clipboard. If that proves unbearable, M4 and M5 dogfood together.

## 7. Red team

Each section above is attacked before it is marked decided. Findings and their resolutions are
recorded here.

### Pass 1 — §4 stack, 2026-10-23

A separately briefed adversarial reviewer, which checked claims against sources.

| # | Finding | Resolution |
|---|---|---|
| F1 | "No C in the path" fails: XKB keymaps need libxkbcommon-x11, which takes an `xcb_connection_t*`, so XCBConnection plus libxcb | Accepted. The input connection uses XCBConnection plus libxkbcommon in one adapter crate; the clipboard path stays pure Rust |
| F2 | gtk3-rs "maintained" overstated: single-maintainer revival | Accepted. Restated, pinned, C + GTK 3 fallback named |
| F3 | gtk3-rs has no SNI; an SNI menu is panel-owned and cannot pop up at the pointer | Accepted. ksni for the tray; the popup is a UI-owned window on an xfconf hotkey |
| F4 | Image previews put gdk-pixbuf on untrusted data | Accepted. The daemon decodes images and sends raw RGBA thumbnails |
| F5 | Safe Rust turns the CVE into a panic, which is still DoS | Accepted. No-panic lints in parsers, fuzzing, size limits, supervised restart |
| F6 | Go has gotk3 | Accepted. Table corrected; verdict unchanged |
| J1 | One X connection mixes clipboard bulk with input | Accepted. Separate connections and threads; blocking poll loops for X |
| J2 | UI language choice not argued against C + GTK 3 or a panel plugin | Accepted. Argued in §4.3; left to the owner |
| J3 | The split is justified by crash isolation, not stalls; the socket is a new attack surface | Accepted. 0700 dir, SO_PEERCRED, fuzzed and versioned protocol |
| J4 | Barriers, grab failure, XTest release, CLIPBOARD_MANAGER, remote timestamps unnamed | Accepted. Listed under "Hard problems" |
| J5 | Transport is a stack decision | Accepted. QUIC via quinn |

### Pass 2 — §5 architecture and §6 milestones, 2026-10-23

A separately briefed adversarial reviewer. Two of its claims were checked on the owner's machine
and corrected. Firefox is an apt package there (156.0), not a snap; Chromium is snap-only. The
owner runs the standalone `xfce4-clipman` process (so it is detectable by name), as well as the
panel plugin variant (which is not).

| # | Finding | Resolution |
|---|---|---|
| F1 | INCR streamed from a remote peer cannot fail cleanly; a drop silently truncates | Accepted. Fetch the whole target, then serve; refuse before any byte, within about 3 s |
| F2 | In M4, nothing triggers pointer arrival while Synergy moves the pointer; both claim ownership | Accepted. Input sharing (M4) before clipboard sharing (M5); Synergy's sharing must be off |
| F3 | clipman runs in the panel's wrapper, so process detection fails | Partly wrong: the owner runs standalone `xfce4-clipman`. Detection by process, xfconf and behaviour |
| F4 | A 2 ms p99 cannot be measured with X timestamps or across clocks; CI timing flakes | Accepted. `CLOCK_MONOTONIC` in the harness; loose CI gate; LAN check for acceptance |
| F5 | Firefox and Chromium are snaps; drivers are unsized | Partly wrong (Firefox is apt here). Non-snap builds; drivers sized in M2 |
| F6 | "Reclaim on restart" contradicts keep-alive | Accepted. Claim only if there is no owner |
| F7 | Pairing contradicts "refuse unpaired"; a code from the keys can be ground | Accepted. Timed pairing mode; code from the TLS exporter; RFC 7250 considered |
| F8 | The password hint is overclaimed | Superseded by owner: no filtering at all; explicit clearing instead (§5.2) |
| J1 | The watchdog releases legitimately held keys; auto-repeat undesigned | Accepted. Held-key heartbeat; suspect at 300 ms and dead after seconds; the secondary repeats |
| J2 | Modifiers and buttons held across a crossing | Accepted. No crossing mid-drag; modifier sync on every switch |
| J3 | Datagram motion reorders against clicks | Accepted. Motion on stream 0 |
| J4 | Keymap hashing gives false mismatches; borrowed keycodes are left by crashes | Accepted. Keycodes plus group; per-key fallback; restore at start and exit |
| J5 | Capturing PRIMARY on its event gets stale text | Accepted. Wait for release; no PRIMARY keep-alive |
| J6 | Keep-alive timestamp | Accepted. The XFixes event timestamp, never `CurrentTime` |
| J7 | CLIPBOARD_MANAGER unspecified | Accepted. Spelled out in §5.2 |
| J8 | Plaintext history; peers get excluded entries; whole-store rewrites | Content-addressed blobs accepted. Filtering superseded by owner: everything persisted and shared; clearing deletes files and propagates |
| J9 | Fetching every target burdens lazy renderers | Accepted. One canonical target per class |
| J10 | The systemd unit has no start point; restart loop at logout | Accepted. Start from autostart after importing the environment; `RestartPreventExitStatus` |
| J11 | Milestones: M1 too big, the matrix comes after dependence, no rollback, import, dogfood, RandR or `switch` | Accepted. M1 split; core matrix in M1; rollback, import, dogfood criteria, RandR, CLI added |

### Owner decision after pass 2, 2026-10-23

"Don't filter clipboard contents at all. I know what's in my clipboard, and if I want to clear my
clipboard of a password then I should have the option." All sensitivity detection, exclusion and
non-persistence was removed. Clearing (clipboard, entry, history), propagated to peers, is a core
requirement in §3 and §5.2.
