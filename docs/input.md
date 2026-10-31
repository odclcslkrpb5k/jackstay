# Input sharing

How one machine's keyboard and mouse drive another's screen (milestone M4). Plan §5.3 says what
input sharing must do; this says how. It rides on the link (docs/link.md).

Status: **design, revised after red-team pass 1** (findings and their resolutions at the end);
slice 1 (the `input` crate, the `proto::link` input messages, `[input]` configuration),
slice 2 (`x11-input`, tested against Xvfb) and slice 3 (the link's input path, the primary's
state machine, the daemon's input thread, `jackstay status` and `switch`, the hotkeys, two-daemon
tests and the latency gate) built. Acceptance on the owner's machines (slice 4) is next.
Facts marked *measured* come from `spikes/x11-probes` (`run-input.sh` and `run.sh`, Xvfb 21.1.22,
2026-10-29), from the reviewer's probes, or were read from the owner's X server (athena,
read-only, 2026-10-29).

## Shape

- One machine has the keyboard and mouse in use: the **primary**. In the first version it is set
  in configuration (`input.primary = true`), and the others are **secondaries**. A secondary
  never starts a crossing; its own mouse and keyboard still work on its own screen.
- Each machine's configuration names its **neighbours**, a peer per side:

  ```toml
  [input]
  primary = true          # athena; false (the default) on xiphos
  left = "xiphos"         # a paired peer: a fingerprint, or a name only one peer has
  right = "xiphos"
  ```

  xiphos names athena on both its sides. Each neighbour is **resolved to a peer's key** when the
  configuration loads and whenever the peers change. A name that no peer has leaves that side off
  until one is paired; a name two peers share leaves it off, and says so in status. From then on
  everything goes by key: a peer's name is only what it called itself at pairing, so a name never
  grants anything. A secondary accepts input only from a peer it resolved as a neighbour, so
  pairing for the clipboard alone never lets a peer type on this machine. The settings window
  writes fingerprints.
- A new crate, `input`, holds the logic with no X in it: layouts, edges, the mapping between them,
  the secondary's virtual pointer, held keys and buttons, spans, and each side's state machine. It
  is tested by property tests and scripted event sequences, as `selection` is.
- A new crate, `x11-input`, is the input thread's X side: its own X connection (separate from the
  clipboard's, plan §4), RandR, XFixes barriers, XInput 2 grabs and raw events, XTest, and XKB's
  lock state. It forbids `unsafe`.
- In the daemon, an **input thread** runs a blocking poll loop over its X connection and a wake
  pipe, as the clipboard thread does. The link reaches it without the clipboard thread: each kept
  session calls input's own callback from the link thread, in order (kept, with a lock-free
  freshness stamp; each input message; silent or heard again; ended), every report naming its
  session. The input thread sends through a handle of its own (`jackstay_net::Sender`), naming
  the session it means, and a message for a session since replaced is dropped, never sent on its
  successor. So a keypress never waits behind the clipboard.

### No C after all

Plan §4 (pass 1, F1) accepted libxkbcommon, and with it libxcb and an `unsafe` adapter, to track
XKB state. This design needs no keyboard state machine: keys travel as keycodes, the secondary's
X server applies its own modifiers, groups and auto-repeat, and the per-key check below needs only
each keycode's keysyms (core `GetKeyboardMapping`) and the modifier map (`GetModifierMapping`).
Locks are set with XKB's `LatchLockState`, which x11rb's `xkb` feature has in pure Rust. x11rb's
`RustConnection` has every request used here (*measured*: the spike uses it for barriers, grabs,
XI2 raw events and XTest). So the input path has **no C and no `unsafe`**, and
`unsafe_code = "forbid"` reaches every crate. That is an Eliminate where the plan had a
Substitute; plan §5.1's `x11-input` row changes with it.

## Layouts and edges

- Each machine reads its **monitors** from RandR (`GetMonitors`) at start, on
  `RRScreenChangeNotify`, on RandR CRTC and output change notifications, and on the root window's
  `ConfigureNotify` (*measured*: on Xvfb, `xrandr --setmonitor` sends only the last). A secondary
  sends them to its primary in a `Layout` message on each new session and whenever they change;
  a primary sends none, so a peer's layout says it is a secondary, ready to be driven, and two
  machines both set as primary never cross to each other. A layout is at most 16
  rectangles in root-window coordinates.
- A machine's **side** (left, right, top, bottom) is the side of its monitors' bounding box. Its
  **edge** on that side is the set of monitor segments that lie on it: athena's left edge is
  x = 0, y 0..2880 (two monitors); xiphos's left edge is x = 0, y 1200..4080 (DP-1-1 and DP-1-8),
  because HDMI-2 starts at x = 939. Edges between a machine's own monitors are never barriers.
- **A point on an edge** travels as a **fraction** of the edge's covered length, gaps skipped
  (u16, in 1/65536), and each side turns it into a point with its *own* current layout. So a
  crossing always lands on a monitor, never in a dead zone, and a layout that changed in flight
  cannot misplace it. athena's left edge (2880 px) meets xiphos's right edge (2880 px covered,
  y 1200..4080) one to one, offset by 1200; a 1440 px edge meeting a 2880 px one maps each pixel to
  two. (Synergy maps over the whole side, dead zones included; the owner asked for these not to be
  entered, so muscle memory near xiphos's top may change. Checked at acceptance.)
- Crossing from the primary's left side enters the secondary on its right side, and so on. The
  owner's wraparound (athena left and right both lead to xiphos) is two neighbours that happen to
  be the same peer.

## Spans

A **span** is one stay of the pointer on a secondary. `Enter` opens it with a span id (u32, new
for each crossing); every later message of the span carries the id, and a message whose id is not
the current span's is dropped, so a late `Leave` can never end a newer span. A span ends, on each
side independently, on:

- `Leave` (the secondary's pointer left through an edge), or `Exit` (the primary took the pointer
  back: the hotkey, `jackstay switch`, a layout change that removed the edge, a grab lost);
- the session being suspect (300 ms of silence), closed or replaced: both sides end the span
  themselves, without waiting for a message.

Every return the primary starts itself also queues `Exit`, so the secondary ends the span even if
it never noticed the silence. And every `Enter` follows an `Exit` with the last span id: a
silence only the secondary noticed (the way to it lost, while the primary still heard it) left it
draining, and this ends that drain after anything late. A message that arrives before this machine's engine has kept the
session (the peer's engine kept it first, and sent its layout at once) is held, up to 16, and
delivered when it does. When a span ends, the secondary releases everything it holds and
ignores the rest of the span's messages.

**The backlog after silence.** After a black hole, QUIC delivers what was sent meanwhile, in
order, and that can include an `Enter` the secondary never saw before the silence, with keys after
it. So when the session goes suspect, the secondary ends its span and **drains**: it drops every
input message until an `Exit`. The primary, when it notices the silence, queues `Exit` with its
last span id whether a span was open or not, so on the ordered stream that `Exit` follows exactly
the backlog. A late key is therefore never injected, however the silence fell. A new session
starts with no backlog and no drain. The primary also sends `Keymap` before its first `Enter` on a
session; until it arrives the secondary injects no key at all, since a keycode's meaning here is
unknown.

**Every `Enter` is answered.** A secondary that takes the span answers `Entered` with the span id
and whether it holds the primary's keymap; without it, the primary sends `Keymap` again. A
primary that has no answer within 300 ms takes the pointer back (an `Exit`, as any return), and
`jackstay status` says the peer did not take the last crossing; the barrier crosses to that peer
again no sooner than 3 s later, so a push against the edge does not grab and let go over and
over, while `jackstay switch` tries at once. Whatever keeps a secondary from taking input (a
rival running there, a drain, a configuration that does not name this primary), the primary
never stays grabbed with nothing driven. A secondary that stops taking input mid-span (a rival
started) releases what it holds and sends `Leave` where the pointer came in. *Seen on the owner's
machines, 2026-10-31:* xiphos still ran `deskflow-core`, took no input, and athena stayed grabbed
until the return key; once it stopped, keys still did not reach xiphos, which had dropped the
keymap sent before that first crossing.

## Primary

- **Barriers.** An XFixes pointer barrier lies on each segment of each side that has a
  neighbour, with XI 2.3 barrier events selected. *Measured:* barriers on the screen's own edges
  report hits when relative motion pushes against them, with the crossing point and the push's
  delta; the barrier for the right side goes at x = width (at x = width - 1 the pointer stops one
  pixel short). Barriers are rebuilt on every layout change.
- **Crossing.** On a barrier hit whose push points outward, while the neighbour's session is
  connected and its layout known, and no button is held (checked with `XIQueryPointer`'s button
  mask; a barrier event carries no button state): grab the master pointer and keyboard with
  `XIGrabDevice`, the pointer with an invisible cursor. Then check the button mask again, and if a
  button went down in between, ungrab and stay. The pointer grab asks for raw motion itself:
  *measured*, raw motion selected on the root does not reach the client that grabs. *Measured:* both grabs take 72 µs; the cursor is a
  1×1 transparent image while grabbed, and the server restores the real one on ungrab, so a crash
  cannot leave it hidden. Then park the pointer at the centre of its monitor (a warp; *measured:*
  a warp adds no raw event), mark the session active (100 ms pings, docs/link.md, "Liveness"),
  and send `Enter` with the span id, the edge fraction, the held modifier keys and the lock state,
  all read after the grabs succeeded.
- **No half switch.** If either grab fails, the other is released and the pointer stays.
  *Measured:* another client's keyboard grab makes ours `AlreadyGrabbed` while the pointer grab
  succeeds; a button held in another client's window makes the pointer grab `AlreadyGrabbed`; a
  button held over the bare root does not (hence the button check). Attempts are at most one per
  50 ms: a push against the barrier while a menu holds a grab makes hundreds of barrier hits a
  second.
- **While on the secondary,** the primary forwards from its grab:
  - **motion** from `XI_RawMotion`'s *processed* values, decoded by the event's valuator mask:
    athena's trackball reports X and Y on valuators 0 and 1, and its scroll wheel on 2 and 3
    (*measured*, `xinput list`), so a wheel turn is a raw motion event with no X or Y in it.
    Events from devices in absolute mode (tablets, touchscreens) are not forwarded as motion.
    Processed values are the motion the pointer would have made here, after this machine's
    acceleration (athena's trackball uses libinput's adaptive profile, *measured*). Fractions are
    kept and carried, not rounded away. *Measured:* raw motion keeps arriving while the pointer is
    pinned at an edge, and `XI_Motion` events from our own warps are ignored. Whether athena's
    processed values really are accelerated is measured at acceptance.
  - **buttons** 1–9 (4–7 are the wheel), from `XI_ButtonPress`/`Release`; XTest has no button
    beyond 10 (*measured*: 12 is `BadValue`). Whether XI 2.4 delivers the wheel's emulated button
    events to a grabbing client, or only scroll valuators, cannot be measured on Xvfb (XTest's
    wheel buttons are real buttons, not emulated). The protocol flags emulated button events to
    XI2 clients (`XIPointerEmulated`, XI2.h), so they are forwarded as buttons; acceptance on
    athena checks a real wheel. If only valuators arrive, the primary turns each scroll step (the
    valuator's increment) into a button 4–7 click.
  - **keys** as keycodes, from `XI_KeyPress`/`Release`, skipping the server's repeats.
    *Measured:* a held key's repeats arrive as `XI_KeyPress` with the repeat flag (9 in a second
    on Xvfb), and raw key events are never repeated.
  The pointer is warped back to the park point when it drifts more than a quarter of the way to
  its monitor's edge, so it never meets a barrier while grabbed.
- **Returning.** On `Leave` for the current span, the primary turns the fraction into a point on
  its own edge, warps there, one pixel inside, ungrabs, and marks the session inactive. A span
  that ends any other way (Spans, above) returns the pointer to where it crossed.
- **Freshness, whatever the link thread says.** Suspect is detected by the link thread; if that
  thread wedged, nothing would report it until the link's 10 s supervision. So the link thread
  also stamps an atomic with the time of each frame it receives, per peer, and the input thread,
  while grabbing, ends the span when the active peer's stamp is more than 300 ms old (it wakes for
  that deadline). The user's keyboard therefore never feeds a silent session for more than
  300 ms, even with the link thread stuck.
- **Hotkeys,** seen while grabbing, consumed, and never forwarded:
  - `input.return_key` (default Scroll Lock, which athena's keyboard has at keycode 78): return
    the pointer at once, a way home if the secondary's screen is off or its daemon wedged.
  - `input.lock_key` (default Scroll Lock with Shift): lock the pointer to the secondary it is on
    until pressed again (plan §3, "lock to screen"). The primary sends `Lock`, and the
    secondary never sends `Leave` while locked, so neither side ends the span by itself.
  Each is written as a key name (`Scroll_Lock`, `Pause`, `Break`, `Print`, `Insert`, `Menu`,
  `F1`–`F24`) or a keysym in hexadecimal, after `Shift+` for one with Shift, or `"none"`
  (docs/daemon.md, "Configuration"); Shift counts if either Shift key is held, including one held
  when the pointer crossed. Outside a grab these keys do nothing special. `jackstay switch PEER` crosses to a neighbour
  without the mouse, entering at the middle of the edge that faces this machine; `jackstay
  switch` alone returns. Either can be bound to keys through Xfce's shortcuts.
- **Watchdog.** A wedged input thread holding the grab would leave this machine with no working
  keyboard or mouse: the worst outcome there is. A **dedicated watchdog thread** (no X, no locks
  shared with anything, a 100 ms tick) holds a duplicate of the input connection's socket
  descriptor. If the input thread holds the grab and has not checked in for 500 ms, the watchdog
  shuts the socket down, and the X server releases every grab and barrier of the closed client
  (*measured* by the reviewer: after `shutdown` on the duplicate, another client could grab both
  devices, and the pointer passed where the barrier had been). Then the daemon exits 101 and is
  restarted, as for the link. A crash of the whole daemon releases the grab the same way. As with
  the link, three failures in a row start input off at the next start, with the reason in status,
  so a recurring fault costs input sharing, never the clipboard.

## Secondary

- **Clearing XTest's held keys.** Slice 2's tests confirm it on every run: a key injected,
  seen down by another client, stays down after the injecting connection closes, and
  `clear_xtest` releases it. *Measured (corrected 2026-10-29):* the X server does **not**
  release keys held through XTest when the injecting connection closes; they stay down and
  auto-repeat (plan §5.3 had said otherwise, from a probe that was Xvfb's only client). So at
  every start, input on or off, and at every `Enter`, the secondary reads the XTest keyboard's and
  pointer's held keys and buttons (`QueryDeviceState` on the "Virtual core XTEST" devices) and
  releases each. *Measured* by the reviewer: the XTest keyboard device reports exactly the keys
  held through XTest, apart from the physical keyboard's. A daemon that crashed mid-keypress
  therefore leaves the key down only until it restarts, which systemd does at once.
- **Virtual pointer.** On `Enter`, the secondary warps to the edge point and from then on keeps
  the pointer's position itself: each `Motion` delta moves it, with fractions carried, and the
  new position is sent as **absolute** XTest motion. *Measured (M0):* absolute XTest motion
  passes through barriers; relative XTest motion would apply this machine's acceleration a second
  time. A position that would leave every monitor is clamped into the monitor it was on, so it
  cannot reach a dead zone. A push past a side of the bounding box that this monitor lies on, with
  a neighbour there, sends `Leave` with the fraction and stops injecting until the next `Enter`;
  but not while an injected button is held, so a drag to the edge stays a drag (plan §5.3). A
  layout change during a span re-clamps the position.
- **This machine's own mouse.** The secondary selects `XI_RawMotion` on the root, which reaches it
  wherever the pointer is (*measured* by the reviewer: `XI_Motion` on the root stops at the first
  window that selects XI2 motion, as GTK's do). Raw motion from any device but the XTest pointer
  marks the position stale, and the secondary asks the server where the pointer is before its
  next injection.
- **Buttons** are injected as they come. **Keys** are injected by keycode when this machine's
  keycode has the same keysyms (group 1, levels 1 and 2) as the primary's; the primary sends its
  keymap's keysyms for those levels, and which of its keycodes are modifiers, in `Keymap`, once
  per session and on `MappingNotify`. A keycode whose keysyms differ is sent as the keycode on this
  machine that has the primary's keysyms, found once per keymap change. One with no match here is
  dropped and logged by keycode (borrowing a spare keycode, plan §5.3, is left until a real key
  needs it), as is a keycode with no keysyms on the primary: nothing says what it means here. This matters on the owner's machines: athena maps Caps Lock to Super (`caps:super`;
  *measured*: keycodes 66 and 133 are both Super_L, and athena has no Caps_Lock key at all); if
  xiphos does not, athena's keycode 66 is sent to xiphos as its Super_L.
- **Auto-repeat** is this machine's own. *Measured:* the X server auto-repeats a key held through
  XTest, so a held key repeats at the secondary's rate, and the primary never forwards repeats.
- **Held keys and buttons** are tracked, and every one is released when a span ends (Spans,
  above). 300 ms of silence is shorter than athena's 500 ms repeat delay (*measured*: `xset q`),
  so a lost release cannot become an endless repeat there; xiphos's delay is checked at
  acceptance.
- **Modifiers and locks.** Modifier bits are not portable (which of Mod2–Mod5 means what differs
  between machines), so `Enter` carries the primary's **held modifier keys** (keycodes the
  primary's modifier map lists, never other keys, which would type), and the secondary presses
  each, translated through `Keymap` as any key is. The **locks** travel by name (Caps Lock, Num
  Lock), and the secondary sets them with XKB's `LatchLockState`, finding Num Lock's real modifier
  in its own modifier map; no lock key is needed, which matters on athena, which has none. On
  `Leave` the primary already holds whatever the user still holds, and the secondary releases its
  copies.

## Messages

Stream 0 (docs/link.md, "Streams"), in order, so a click never overtakes the motion before it.
New `proto::link` kinds, each with its own limit; the link protocol version becomes 2 (both
machines update together, as for control v3), and 3 with `Entered`.

| Kind | Way | Payload |
|---|---|---|
| `Layout` | both | 1 to 16 monitors, each x, y, width, height (u16; none empty, none past 32767) |
| `Keymap` | primary → secondary | first keycode, count (≤ 248), two keysyms (u32) and a modifier flag each |
| `Enter` | primary → secondary | span (u32), side, edge fraction (u16), held modifier keys (32-byte bitmap), locks (u8) |
| `Motion` | primary → secondary | span, dx, dy in 1/256 px (i32 each) |
| `Button` | primary → secondary | span, button (u8), down (u8) |
| `Key` | primary → secondary | span, keycode (u8), down (u8) |
| `Leave` | secondary → primary | span, side, edge fraction (u16) |
| `Exit` | primary → secondary | span; also the end of what arrives late after silence (Spans) |
| `Lock` | primary → secondary | span, on (u8): the lock key locked the pointer to the secondary |
| `Entered` | secondary → primary | span, keymap (u8): the span is taken, and the primary's keymap held or not |

Decoding refuses a side, button, keycode, lock bit or monitor count out of range; a monitor with
no area or past 32767; a keymap running past keycode 255; an up/down or modifier flag other than 0
or 1, so every frame has one encoding; and `Motion` beyond ±65,536 px. A secondary drops input
messages from a peer that is not its neighbour, and any message whose span is not the current
one; of `Enter`'s held keys it presses only those the primary's `Keymap` flags as modifiers.

## Coexistence

Input sharing refuses to start while a process named `synergy`, `synergys`, `synergyc`,
`synergy-core`, `synergy-server`, `synergy-client`, `deskflow`, `deskflow-server`,
`deskflow-client`, `deskflow-core`, `barrier`, `barriers`, `barrierc`, `input-leap`,
`input-leaps` or `input-leapc` runs (plan §5.6; athena runs `synergy` and `synergys`,
*measured*), says so in `jackstay status`, and checks again every 10 s, so stopping Synergy is
enough to start it. Synergy's own barriers and grabs would otherwise fight ours. The clipboard is
unaffected. A rival whose environment names another X display than ours is ignored: it grabs
nothing of ours (and the tests run one rival per Xvfb). One whose environment cannot be read
(another user's), or names no display, counts.

## In the daemon

- `jackstay status` shows input's state: off and why, waiting for a neighbour, here, on a peer,
  or locked to a peer; on a secondary, which primary can drive it and whether it does now
  (control version 5, `Input`; docs/daemon.md). The journal says when the pointer crosses and
  returns, by peer fingerprint; never keys or positions (DP-3).
- Neighbours are resolved on the input thread whenever the clipboard thread sees the peers
  change. A secondary's neighbours must all be one peer, its primary; it takes input from no
  other. The input thread counts its failures in a row (`input-failures` in the state directory)
  as the link counts its own: three, and input is off at the next start; a minute running forgets
  them.
- `jackstay switch [PEER]` (Primary, above).
- Configuration gains `[input]` (`primary`, `left`, `right`, `top`, `bottom`, `return_key`,
  `lock_key`); the settings window keeps it as it keeps the link's keys.
- After `Leave`, the secondary's cursor stays where it left, at the edge, as Synergy's does.

## Latency

- The path: X event → input thread → channel and wake → link thread → stream 0 → UDP → link
  thread → channel and wake → input thread → XTest, flushed at once. Nothing on it allocates per
  event beyond the frame, and nothing on it takes a lock the clipboard holds.
- **CI gate:** in the two-daemon harness, the time from injecting a motion on A's Xvfb to B's
  Xvfb reporting it, both stamped with `CLOCK_MONOTONIC` in the test process (plan §5.7).
  *Measured* (2026-10-30, ten runs on athena, 200 motions each, beside the other input tests):
  median 0.39–0.56 ms, p99 0.6–2.2 ms. The gate: median under 20 ms, p99 under 100 ms.
  Xvfb applies `XTest` input when it gets to it, and now and then not at all (*measured*: a
  3-pixel nudge during the grab left the pointer where it was, and no event came); so the test
  nudges one way only, and a nudge that shows nothing within 500 ms is counted, not timed (none
  so far; at most ten allowed).
- **Acceptance on the owner's LAN** (plan §6, M4): added latency p99 under 2 ms, measured as half
  of `Motion` echo round trips through a debug-only echo mode, beside the link's own ping round
  trips. Measured once, recorded here.

## Testing

- `input` (no X): property tests that a mapped crossing always lands on a monitor, that crossing
  out and straight back returns within a pixel, that the virtual pointer never leaves the
  monitors, that every held key is released by each way a span ends, and that a stale span's
  messages change nothing; scripted sequences for each state machine.
- `x11-input` against Xvfb, one machine: barriers per layout (RandR monitors made with
  `xrandr --setmonitor`, so the owner's layouts are reproduced), the grab and its refusals, the
  button checks, the invisible cursor, injection, clearing XTest's held keys. Every Xvfb in these
  tests keeps a second client connected, so a reset never hides a stuck key. Xvfb has one CRTC, so
  the server's own confinement to monitors is not exercised; the virtual pointer's clamping is
  what keeps out of dead zones, and it is tested without X.
- Two daemons through the relay (docs/link.md, "Testing"), driving A with relative XTest motion
  (a real mouse's kind) and asserting on B: crossing both ways on the owner's two layouts, the
  mapped point, keys and modifiers across a crossing, a key held across a black hole released
  within 300 ms and never repeated after, the backlog after a black hole dropped, B SIGKILLed
  mid-key (the key released when B restarts), a link lost while on B returning A's pointer, A's
  link thread stopped (`SIGSTOP` is not selective, so a test hook) returning the pointer within
  300 ms, no crossing with a button held, a drag on B held at its edge, refusal while a
  Synergy-named process runs, the watchdog (input thread stopped while grabbing), the hotkeys, and
  the latency gate.
- Plants for each control, as in M3.

### Built in slice 3

- `jackstay-net`: per-session input reports (`Input`), `Handle::send` and `Sender`, held early
  messages, `Heard`; four tests (in order both ways; only input, only for the session kept; silence,
  freshness and end; a message sent the moment a session is kept).
- `jackstay-input`: the primary's state machine (`Primary`), twenty scripted tests and a property
  test that crossing out and straight back lands within a pixel; the secondary follows this
  machine's own mouse (`Secondary::moved`).
- `jackstayd`: the input thread, coexistence with Synergy and its successors, `[input]`
  `return_key` and `lock_key`, control version 5; ten two-daemon tests: crossing out and back
  on the owner's layouts, keys, a black hole releasing a held key within its 300 ms and never
  pressing it again, loss from the secondary alone, no crossing with a button held, the return
  key and `jackstay switch`, input off while `synergys` runs and on within 10 s of its end, a key
  held when the secondary is killed released when it restarts, crossings after idle spells, loss
  toward the secondary alone, and the latency gate.
- Not yet tested end to end: the freshness check with a wedged link thread, and the watchdog with
  a wedged input thread (each needs a hook to wedge a thread; each is tested on its own, the
  freshness rule in `Primary`, the watchdog against Xvfb in slice 2).

## Acceptance

On athena and xiphos, with Synergy stopped: the owner uses Jackstay for input (plan §6), after
these are measured and recorded here: added latency p99; whether athena's processed motion is
accelerated (a read-only XI2 listener while the owner moves the trackball); that the trackball's
wheel scrolls on xiphos; crossings near xiphos's top, against Synergy's habit. Already read
(2026-10-29): xiphos has a US layout without `caps:super`, a 500 ms repeat delay, and no pointer
device of its own.

*Owner's report, 2026-10-31,* after the dogfooding fixes below: motion "performs at least as well
as Synergy"; the trackball's wheel "works great on xiphos"; xiphos's top monitor does not cross on
its edges, as expected (athena is beside xiphos's lower monitors only). Still to measure: the
added latency p99 on the LAN, and whether athena's processed motion is accelerated.

## Slices

1. The `input` crate (layouts, edges, fractions, virtual pointer, spans, held state), the
   `proto::link` input messages with their fuzz target, and the `[input]` neighbours and
   `primary` in the configuration.
2. `x11-input` and the input thread on one machine: barriers, grabs, button checks, injection,
   XTest clearing, RandR, the watchdog, tested against Xvfb.
3. Wiring to the link (freshness stamps, spans over stream 0, the drain's `Exit`), neighbours
   resolved to keys, the two-daemon tests, `jackstay status`/`switch`, the hotkeys and their
   configuration (`return_key`, `lock_key`), the latency gate.
4. Acceptance on the owner's LAN, then dogfood.

## Red team

### Pass 1, 2026-10-29

A separately briefed reviewer, which ran its own probes in the dev container and read athena's X
server read-only.

| # | Finding | Resolution |
|---|---|---|
| 1 | XTest-held keys stay down after the injecting client dies; M0's opposite result came from Xvfb resetting when its only client left | Confirmed (`run.sh` now keeps a second client). Plan §5.3 corrected; the secondary clears XTest's held keys at start and on every `Enter`; tests keep a second client |
| 2 | Span end under-specified; a late `Leave` or a post-black-hole backlog can strand a key | Span ids; suspect, close and replace end the span on both sides; the primary queues `Exit`; stale messages dropped |
| 3 | 300 ms depends on the link thread, which only a 10 s supervisor watches | Per-peer freshness stamp (atomic) read by the input thread |
| 4 | The watchdog in the clipboard loop is slow (1 s sleeps) and shares its stalls | Dedicated watchdog thread, 100 ms tick; three failures turn input off |
| 5 | `XI_Motion` on the root does not see motion over windows that select XI2 | `XI_RawMotion` by source device; re-query the pointer |
| 6 | Neighbours by name: any peer calling itself "athena" could type | Resolved to keys; ambiguous names leave the side off; fingerprints in the settings window |
| 7 | No mechanism for "no crossing mid-drag"; a grab over the root succeeds with a button held; drags out of the secondary | `XIQueryPointer` button mask before and after grabbing; no `Leave` while an injected button is held |
| 8 | Modifier bits are not portable; athena has no Caps_Lock key to toggle | Held modifier keys through `Keymap`; locks by name with XKB `LatchLockState` |
| 9 | Raw motion includes scroll valuators | Decode by valuator mask; skip absolute devices; wheel handling measured in slice 2 |
| 10 | Points computed against a stale peer layout | Edge fractions, mapped by each side with its own layout; re-clamp on change |
| 11 | Xvfb `--setmonitor` sends only `ConfigureNotify`; one CRTC | Listen for it too; say what Xvfb cannot cover |
| 12 | Return key consumed?; lock-to-screen and cross-to keys missing | Consumed, never forwarded; `lock_key`; `jackstay switch PEER` |
| 13 | Grab retries per barrier hit | At most one per 50 ms |
| 14 | Synergy's GUI and forks' process names | Full list |
| 15 | Cursor left visible at the secondary's edge; Synergy's mapping differs | Kept, as Synergy does; mapping difference checked at acceptance |

### Pass 2 (code review of slice 1), 2026-10-29

A separately briefed reviewer, which verified each finding with a scratch test.

| # | Finding | Resolution |
|---|---|---|
| 1 | Locked to the secondary, a push past its edge still sent `Leave` and ended the span there, leaving the primary grabbing with nothing accepting input | `Lock` message; no `Leave` while locked |
| 2 | After silence, the backlog can hold an `Enter` the secondary never saw, and keys after it, injected seconds late | Drain after suspect until the primary's `Exit`, which it sends on noticing silence, span or not |
| 3 | Before `Keymap`, keycodes passed through untranslated (athena's Super as xiphos's Caps Lock) | No key until `Keymap`; the primary sends it before any `Enter` |
| 4 | A keycode with no keysyms on the primary passed through, typing whatever it is here | Dropped |
| 5 | A move longer than a monitor clamped to the monitor it started on, missing the one it reached and its edge | The target clamped into the bounding box picks the monitor |
| 6 | The document claimed neighbour resolution and hotkey settings in slice 1 | Moved to slice 3 |

### Pass 3 (code review of slice 2), 2026-10-29

A separately briefed reviewer, which verified findings 1 and 2 with probes on Xvfb.

| # | Finding | Resolution |
|---|---|---|
| 1 | An XKB client gets no core `MappingNotify`, and nothing for a new keyboard: `xmodmap` and `setxkbmap` went unreported, so a changed keymap would stay stale until restart | XKB `MapNotify` and `NewKeyboardNotify` selected. *Measured*: a new keyboard is also announced each time typing moves from one keyboard device to another, with the same details as `setxkbmap`'s, so any of these re-reads the map and reports it only if it changed |
| 2 | XTest's raw motion can be absolute (`xdotool mousemove`): an absolute move came through as a jump of hundreds of pixels | While grabbing, XTest's motion is measured from the pointer's positions, not its raw values |
| 3 | The watchdog read the beat before the grab flag, and could shut the socket right after a first grab | It reads the flag first; the beat is stored before the flag |
| 4 | A call that waits for a reply reads waiting events into x11rb's queue, where `poll` cannot see them | Documented on `Display::fd`: the loop calls `events` after any such call and before every poll (slice 3) |
| 5 | An X error on the keyboard grab left the pointer grabbed, unseen by the watchdog | Every way out after the pointer grab releases it |
| 6 | The repeat test never showed a repeat happened | It now proves the server repeats the held key, then that the grab forwards it once |
| 7 | Two tests relied on sleeps for ordering between clients, and one on a tight bound | A round trip after setting barriers; 1.5 s for the watchdog |

### Pass 4 (code review of slice 3), 2026-10-30

A separately briefed reviewer, reading only.

| # | Finding | Resolution |
|---|---|---|
| 1 | Freshness was measured from the last frame, which on an idle session is up to a second old: about half of all crossings ended at once, and `jackstay switch` failed as often | Measured from the crossing at the latest; a test switches after idle spells and stays on the peer |
| 2 | Loss toward the secondary alone, while idle, left it draining for good (the primary still heard it, and sent no `Exit`); the next crossing grabbed with nothing driven | Every `Enter` follows an `Exit` with the last span id; a test drops traffic toward B for 4 s, then types on B |
| 3 | A secondary whose primary changed mid-span released nothing: the old state was replaced before its keys were let go | The old span's keys and buttons go up first |
| 4 | A layout with no monitor (every output off) failed the input thread, and the daemon with it | Ignored after start (the last layout stands); at start, input is off with the reason |
| 5 | The 10 s scan for a rival read every process's name on the input thread, even while driving a peer; and a layout change while a rival ran put the barriers back | No scan while the pointer is on a peer; no barriers while asleep |
| 6 | Two machines both set as primary saw each other's layout and crossed into a peer that ignores `Enter` | A primary sends no layout |
| 7 | Tests that retried hid finding 1, and nothing lost traffic toward B alone | The two tests above, each watched failing without its fix |

### Slice 2 plants, 2026-10-29

Fifteen bugs planted in `x11-input`, one at a time; thirteen were caught (three of them after
the review above). The two button checks
around the grab mask each other, and both stay:

- Without the check **before** grabbing, a grab taken with a button held over the root is
  released at once by the check after, so no test sees a difference. The check stays because a
  button released during that brief grab would go to us, not to the application, which would be
  left mid-drag.
- Without the check **after** grabbing, the button checked before is still caught. Only a button
  pressed between the two requests reaches it, which a test cannot time.

### Slice 3 plants, 2026-10-30

Each control's removal planted, and the tests run.

- Link (`jackstay-net`, 6): 5 caught (the session id on sending, input kinds only, the freshness
  stamp, silence reported, the end reported). Survives, accepted: holding the messages that
  arrive before the engine keeps the session. The window is microseconds (the answering side
  keeps first, and its message makes a round trip through the test before it is sent); without
  the hold, a lost layout would leave the primary unable to cross until the next session.
- Primary (`jackstay-input`, 19): 18 caught. The survivor, clearing a grab asked for when the
  session ended, was redundant (the grab's answer finds no session, and lets go) and is removed.
- Daemon (`jackstayd`, 10): 6 caught (keys left held cleared at start, the rival check, the
  session marked active, keys forwarded, the leave sent, the secondary's layout on a new session).
  Of the four that survive, one is the flag below; three are accepted: the freshness check and the watchdog's feeding (both need a wedged
  thread, above), and sending the layout when the primary is first resolved after its session
  was kept (in these tests the peers always arrive first; a name that resolves later, after a
  third machine is unpaired, needs it).
- A secondary drains only after silence, and the design is that a secondary is never the faster
  pinger, so it suspects only when it has heard nothing from its primary: a flag for which kind
  of silence was built, planted, survived, and removed as unreachable.

### Dogfooding 1, 2026-10-31

The owner's first crossings, athena to xiphos.

| # | Finding | Resolution |
|---|---|---|
| 1 | xiphos still ran `deskflow-core`, so it took no input, but its layout said it could; athena grabbed and stayed grabbed with nothing driven, until the return key | `Entered` answers every `Enter`; no answer within 300 ms takes the pointer back, and status says why (Spans) |
| 2 | Once it stopped, keys did not reach xiphos: the keymap went out before the first crossing, while xiphos dropped everything, and was never sent again | `Entered` says whether the keymap is held; without it, the primary sends it again |
| 3 | A rival starting on the secondary mid-span left the primary grabbed the same way | The secondary releases what it holds and sends `Leave` |
| 4 | The tests ran a rival only on the primary, and the fake rival (a copy of uutils' multi-call `sleep`) exited at once: it was seen only as a zombie | Rivals are scoped to their X display, so a test runs one on B alone; the fake rival is a looping shell. Each of the three fixes removed, its test fails |
