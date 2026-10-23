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
- History: browse, search, re-select, pin, delete; exclude sources (e.g. password managers).
- Remote clipboard: the content follows the user to the machine they move to, and is offered
  there under the same targets.

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
   from a malformed clipboard update, in its clipboard chunk assembly.
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
  clipboard contents.
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
- **Tray:** a StatusNotifierItem through [ksni](https://crates.io/crates/ksni) (pure Rust, zbus).
  gtk3-rs has no SNI support. Xfce 4.20's systray hosts SNI items with dbusmenu menus, but the
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

That is **not** the end of the parsing problem. CVE-2026-41476 in safe Rust would be a panic, and
a panic kills the daemon: it drops the clipboard and the input link, which is a remotely
triggerable denial of service against targets 1 and 3. So Engineer controls still apply to every
decoder (wire messages, selection data, images):

- clippy's `indexing_slicing`, `arithmetic_side_effects` and `unwrap_used` denied in parser
  modules, so a parser cannot panic on input;
- cargo-fuzz on every decoder, with its seed corpus run as ordinary tests (TS-16);
- size limits enforced before allocation;
- Protect: a systemd user unit restarts the daemon, and on restart it reclaims selection ownership
  and re-sends key releases.

**Costs we accept, and their controls:**

- **No Rust appendix exists.** We would propose one to the standard's owning session, built from
  what this project proves, as the C appendix was. Until then the core applies as written where it
  can, and each mapping we use is recorded in `CLAUDE.md`.
- **Crate supply chain** (TC-3, TC-4, TC-7): `Cargo.lock` committed, `--locked` builds,
  `cargo-deny` for advisories, licenses and sources, a minimal dependency list with each crate
  justified. quinn and tokio are the largest; they buy the transport above.
- **C in the input path** (libxcb, libxkbcommon), in one adapter crate, as above.
- **A second local protocol** (the UI socket), which is a parser too: fuzzed like the others, and
  versioned from its first message.

#### Hard problems this stack must handle

These shape the design (§5) more than the stack, but the stack has to reach them:

- **Stuck keys.** Find out by experiment whether keys pressed through XTest stay down after the
  injecting client dies or the link drops. Either way: release everything held on disconnect and
  on screen switch, plus a watchdog.
- **Grab failure.** Active grabs fail with `AlreadyGrabbed` while a menu is open or the screensaver
  holds a grab. The pointer needs a defined behaviour when it cannot leave.
- **Remote ownership.** While the user works on another machine, the local daemon must own the
  selection with a correct timestamp, taken from a zero-length property append, since there is no
  user event to take one from. Targets are offered at once and fetched lazily over the network, so
  large items need a latency policy.
- **Coexistence.** xfsettingsd 4.20 has no clipboard manager, but xfce4-clipman, Synergy and
  Deskflow must be disabled while Jackstay runs, or selection ownership and grabs flap. Detect them
  and refuse to start.

### 4.4 Decisions

- **Rust**, approved by the owner 2026-10-23.
- **UI: gtk3-rs**, the recommended default. Fallback: C + GTK 3 behind the same socket protocol.
- A Rust appendix is proposed to the standard's owning session; until it exists, mappings are
  recorded in `CLAUDE.md`.

## 5. Architecture

To be written after §4.

## 6. Milestones

To be written after §5.

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
