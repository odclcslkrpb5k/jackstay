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

**Open — the next step.** The house standard, [Paranoid
TypeScript](https://github.com/odclcslkrpb5k/paranoid-typescript), has an appendix for C + GTK 3;
using that stack must be justified, and alternatives are welcome if they serve the targets better.

Criteria, in order: how well the stack serves each optimization target (§2); how much of the
standard's control apparatus it lets us push up the ladder (Eliminate/Substitute rather than
Engineer/Administer); maturity of its X11 (XFixes, XInput2, XTest, selections/INCR) and Xfce panel
integration; and how much of the standard already carries over to it.

Evaluation, candidates and recommendation: to be written.

## 5. Architecture

To be written after §4.

## 6. Milestones

To be written after §5.

## 7. Red team

Each section above is attacked before it is marked decided. Findings and their resolutions are
recorded here.
