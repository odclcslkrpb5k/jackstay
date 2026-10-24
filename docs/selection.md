# The selection engine

How `jackstay-selection` speaks the X selection protocol (ICCCM §2, with XFixes). This is the
part of Jackstay that decides whether target 1, a flawless clipboard, is met. Plan §5.2 says what
it must do; this says how.

Status: **design, revised after red-team pass 1** (findings and their resolutions at the end).
Source references are to gtk-3-24, GTK main, qtbase dev, the X server (mirror/xserver), Chromium,
Emacs and xfce4-clipman-plugin, as read on 2026-10-24.

## Shape

A pure state machine. It never touches a socket. It is driven by:

- **events** from the X server, already decoded by the adapter (`x11-clip`): XFixes
  selection-owner changes, `SelectionNotify`, `SelectionRequest`, `SelectionClear`,
  `PropertyNotify`, `DestroyNotify`;
- **commands** from the daemon: serve this content, clear;
- **time**: every call carries the monotonic time in milliseconds, and the engine reports its next
  deadline, which the adapter's poll loop honours.

It acts on X through a **port** trait the adapter implements:

- *queries* (a round trip, answered synchronously): read a whole property (bounded; see the
  adapter's guarantees), name an atom, intern a name, get a selection's owner, get a server
  timestamp, read window properties for attribution;
- *requests* (sent, not waited for): convert a selection, change or delete a property, send a
  `SelectionNotify`, set a selection's owner, select or clear property and structure events on a
  window, send the `MANAGER` client message.

Tests drive the engine through a fake port with scripted events. The conformance matrix (plan §5.7)
drives the real adapter against real applications. Property data from other programs is hostile,
so the crate denies every panicking construct, as `proto` does, and is fuzzed on event sequences.

The engine owns one window, created by the adapter with `PropertyChangeMask`.

## Properties for incoming transfers: a pool, never reused while in doubt

Each conversion the engine asks for uses a property atom from a pool (`JACKSTAY_P0`, … interned as
needed, eight before any is reused, then round-robin, at most 256) that no other transfer is using
or might still write to. A reply is accepted only if it matches the conversion it answers on
(selection, target, property, time). Every conversion in a capture is asked with the capture's
selection time, and owners echo it. A reply naming our property may also carry `CurrentTime`, but a
refusal (property `None`) must carry exactly the conversion's time: otherwise a refusal of an older
conversion of the same target, sent by a newer owner that saw our older request, would be taken as
the newer owner's answer. A property whose conversion was abandoned (superseded, timed out) is
**quarantined** for 60 s before reuse. Owners keep going long after we stop listening: GTK keeps a
transfer alive for 30 s (gtkselection.c:140), Chromium for 10 s (selection_owner.cc:46). A late
reply, or a late INCR chunk on a reused property, would otherwise be read as part of a newer copy.

## Ownership states, per selection

- **Theirs(owner, selection time)**: another client owns it.
- **Ours(content, since)**: we own it, and serve `content`; `since` is the time we claimed with.
- **Ours(cleared, since)**: we own it and offer only `TARGETS` and `TIMESTAMP`, so a paste
  yields nothing. This is "clear clipboard" when another application still owns it.
- **Unowned.** Also the state after `SelectionClear`, until the `SetSelectionOwner` notification
  that follows it arrives.

**Events judged by time.** Queries answer with the server's state now; the events being handled
may be older. While we own CLIPBOARD, a `SetSelectionOwner` notification or `SelectionClear` whose
time is earlier than our claim is stale: the server has already put it behind us, and acting on it
would make the engine refuse pastes of what it owns. A death notification counts only if its
selection time is that of the owner the engine believes holds the clipboard.

**Every transition settles what is pending.** Whenever ownership changes (a new owner, a death, a
claim of ours, `SelectionClear`): the capture is abandoned and its property quarantined, and the
daemon is told if it had reported content not yet complete (`CaptureEnded`); the liveness probe
stops and its property is quarantined; and a pending `SAVE_TARGETS` is answered (`None` if the
clipboard moved on, nothing if its requestor is the owner that died).

CLIPBOARD is captured, kept alive and served. PRIMARY is neither captured nor kept alive in M1
(plan §5.2), only cleared (below); the engine is written per selection, so turning it on later is
configuration.

## Targets the engine never converts

Converting some targets has side effects in the owner, or yields nothing worth keeping. On every
path (capture, `SAVE_TARGETS`, "everything you can"), the engine never converts:

`TARGETS` `MULTIPLE` `TIMESTAMP` `SAVE_TARGETS` `DELETE` `INSERT_PROPERTY` `INSERT_SELECTION`
`LENGTH` `DRAWABLE` `BITMAP` `PIXMAP` `COLORMAP`

`DELETE` is the dangerous one: Emacs lists it, and converting it clears Emacs's selection
(select.el:721-728). clipman keeps a similar list (clipboard-manager-x11.c:108-114).

## Capture

Triggered by an XFixes `SetSelectionOwner` notification whose new owner is not our window. Any
such notification, including one whose owner is None, **ends the keep-alive candidate** (below)
and abandons a capture in progress for that selection. A new capture is reported at once
(`CaptureStarted`), before any content, so the daemon knows the clipboard has moved on from
whatever entry it was showing.

1. **Attribute the source.** The owner window is usually a hidden helper window with no `WM_CLASS`
   (Qt's, GTK's, Chromium's). Try its `WM_CLASS`, then the `WM_CLASS` of its
   `WM_CLIENT_LEADER`, then `_NET_WM_PID` (to a process name), then the `WM_CLASS` of
   `_NET_ACTIVE_WINDOW`.
2. **Convert `TARGETS`** with the notification's selection timestamp, never `CurrentTime`. Qt
   checks that time (qxcbclipboard.cpp:530). Accept a reply of type `ATOM` or `TARGETS`, format 32.
   Drop `None` and duplicate atoms.
3. If `TARGETS` is refused or times out, fall back to `UTF8_STRING`, then `STRING`, then
   `image/png`. Some old clients never implemented `TARGETS`.
4. **Choose what to fetch.** Name the offered atoms (cached, since atoms never change). Fetch, in
   this order, so text lands first:
   - text: `UTF8_STRING`, else `text/plain;charset=utf-8`, else `STRING`;
   - every other offered `text/*` target: HTML, RTF, `text/uri-list`, `text/x-moz-url` and the
     like are small and are what rich paste needs. Not `text/plain` or
     `text/plain;charset=utf-8` once a text is chosen: they are the same text again (GTK 3 and
     Qt offer both beside `UTF8_STRING`), and Serving answers them from the text kept. Fetching
     them kept a text two or three times over (found by the conformance matrix);
   - `x-special/gnome-copied-files`, `chromium/x-web-custom-data`;
   - vector images: `image/svg+xml`, `image/x-inkscape-svg`;
   - one raster image: `image/png`, else the first `image/*`. Jackstay serves other image types
     by transcoding, so it does not force the owner to render each.
   Everything else offered (application formats, extra raster types, the never-convert list) is
   recorded as `NotCanonical`. It stays available while the owner lives, because the owner
   serves it, and is fetched as a whole only by `SAVE_TARGETS`.
5. **Convert each chosen target** into a fresh pool property.
   - Deadlines: 10 s for an owner to answer a conversion (LibreOffice and GIMP render inside the
     handler), 5 s between INCR chunks, and 60 s for a whole capture. A deadline missed marks
     that target `OwnerFailed`, and the capture moves on. A hung owner can only cost its own
     capture.
   - **INCR receive.** A reply of type `INCR` gives the size. The adapter's read deletes the
     property, which tells the owner to send. Each `PropertyNotify(NewValue)` on that property is
     a chunk: read it whole, then delete. A zero-length chunk ends the transfer. `NewValue`
     notifications that arrive before the conversion's `SelectionNotify`, and the `Delete`
     notifications our own reads cause, are ignored.
   - Past the per-target byte limit, the target is `TooLarge`. The engine deletes the property
     explicitly (a bounded read does not). An owner that wrote a whole property is finished with
     it, so it is released; an INCR owner may still be sending, so its property is quarantined.
     A `TARGETS` reply of type `INCR` is treated as a refusal, and its property quarantined.
6. **Publish progressively.** The first time a target is kept, the capture is published to the
   daemon as content. Each later target updates it, so a slow image never holds the text back,
   and a capture cut short still has what it got.

## Keep-alive

The **keep-alive candidate** is the latest captured content, tagged with the selection timestamp of
the `SetSelectionOwner` notification that started its capture. It is ended by any
`SetSelectionOwner` notification from another client, owner None included. Otherwise a
copy-and-quit tool whose capture kept nothing would leave the previous copy to be served as if it
were current.

Triggered by an XFixes `SelectionClientClose` or `SelectionWindowDestroy` notification for
CLIPBOARD whose selection timestamp matches the candidate's tag. These notifications carry owner
`None` (xfixes/select.c sets `owner` only for `SetSelectionOwner`), so the owner window cannot be
matched. The selection timestamp can: the server leaves it unchanged at death, so it equals the
time the dead owner claimed with. Only one of the two notifications fires per death, because the
first clears the selection's window and client (dix/selection.c, `DeleteWindowFromAnySelections`,
`DeleteClientFromAnySelections`).

- Claim CLIPBOARD with the notification's **selection timestamp** (`lastTimeChanged`), not its
  event timestamp. When an owner dies the server leaves `lastTimeChanged` unchanged
  (dix/selection.c; only `SetSelectionOwner` sets it, line 217), and refuses any claim earlier
  than it (line 181). So any client that claimed after the death always beats ours, and ours can
  never overwrite it. The event timestamp is the current time, which could.
- A capture still in progress when its owner dies keeps whatever it already published.
- An owner that already held CLIPBOARD when the engine started is captured, but its claim time is
  unknown, so its death cannot be matched and it is not kept alive. A daemon started at login
  normally starts before any application has copied.
- The selection becoming unowned *without* the owner dying (an application releasing it
  deliberately) is not kept alive. clipman's restore-on-empty fights deliberate clears.

## CLIPBOARD_MANAGER

At start, the engine claims `CLIPBOARD_MANAGER` with a server timestamp, and sends the ICCCM
`MANAGER` client message to the root window. If another client already owns `CLIPBOARD_MANAGER`
(xfce4-clipman's daemon does, with `CurrentTime`, and never answers), another clipboard manager is
running. The engine reports that and does not claim it, which is one of plan §5.6's coexistence
checks.

A `SelectionRequest` on `CLIPBOARD_MANAGER` for `SAVE_TARGETS` comes from an application that is
about to exit. GTK 3 waits 10 s for the answer (gtkclipboard.c:2176) and Qt 5 s
(qxcbclipboard.cpp:190), and Qt turns INCR off while closing (qxcbclipboard.cpp:446-449). So:

0. A `SAVE_TARGETS` that arrives before the owner has answered `TARGETS` (an application that
   copies and quits at once) is kept, and merged into the capture once `TARGETS` is answered.
1. **Is the requestor the CLIPBOARD owner?** Qt sends `SAVE_TARGETS` when it owns CLIPBOARD *or
   only PRIMARY* (qxcbclipboard.cpp:219-229). If `GetSelectionOwner(CLIPBOARD)` is not the
   requestor's window, reply success at once and take nothing: claiming would steal CLIPBOARD
   from a live application.
2. **Which targets.** A missing property (GTK 3 sends `None` when its list is empty,
   gdkdisplay-x11.c:2486-2507; Qt deletes it first) or an empty list means everything offered.
   Either way, the never-convert list is removed, and so, once a text is kept or on its way, are
   the text names Serving answers from it (`UTF8_STRING`, `text/plain;charset=utf-8`, `STRING`,
   `text/plain`, `COMPOUND_TEXT`, `TEXT`): the owner would render the same text again, and GTK 3
   takes over 20 s to render 20 MiB of non-ASCII text as `text/plain` while the application
   waits to exit (found by the conformance matrix).
3. **Fetch, within a 4 s budget,** reusing everything the capture of this owner already holds and
   extending that capture rather than starting a second one. On budget expiry, stop with what is
   held.
4. **Claim CLIPBOARD** with the request's timestamp, write a zero-length property of type `NULL`
   on the requestor, and reply with that property. If the request's property was `None`, use
   `SAVE_TARGETS`. GTK 4 treats a reply property of `None` as failure
   (gdkclipboard-x11.c:471-475). If nothing could be kept, reply `None`.

A `TARGETS` request on `CLIPBOARD_MANAGER` is answered `TARGETS, SAVE_TARGETS, TIMESTAMP`.
Losing `CLIPBOARD_MANAGER` (`SelectionClear`) means another manager started; it is reported.

## Serving

A `SelectionRequest` for a selection we own:

- **Refuse** (`SelectionNotify` with property `None`) if we do not own it, if the request's time
  is earlier than `since` (and not `CurrentTime`), or if the target is not offered.
- A property of `None` comes from an obsolete client, and means "use the target as the
  property".
- `TARGETS`: every held target and every alias below, plus `TARGETS`, `MULTIPLE` and `TIMESTAMP`
  (`ATOM`, 32).
- `TIMESTAMP`: `since` (`INTEGER`, 32).
- `MULTIPLE`: read the requestor's `ATOM_PAIR` list (format 32, even length), serve each pair as
  if requested alone, write `None` in place of each failed pair's property, write the list back,
  then notify once.
- **Text aliases.** Holding `UTF8_STRING` (or any one text form) also serves:
  - `UTF8_STRING`, `text/plain;charset=utf-8`;
  - `STRING`, as Latin-1, only if every character is in Latin-1;
  - `text/plain`, only if the text is ASCII;
  - `COMPOUND_TEXT`: Latin-1 text as is; otherwise UTF-8 inside an `ESC % G` … `ESC % @`
    segment;
  - `TEXT`, replied as `STRING` when the text is Latin-1, else as `COMPOUND_TEXT`, as GTK does
    (gtkselection.c:1738-1744). Old clients that ask for `TEXT` do not understand
    `UTF8_STRING`.
- **Image aliases** (plan §3, failure 1). Holding one raster image also serves `image/png`,
  `image/bmp`, `image/jpeg`, `image/tiff` and `image/x-portable-pixmap`, made by transcoding.
  Transcoding runs off the engine on a worker thread, with a deadline. A request for an alias not
  yet made waits within that deadline (the reply is deferred), so a 33 MB encode never blocks the
  engine. If the deadline passes, the request is refused. Transcoding lands in its own slice
  after the engine.
- **Data** up to the INCR threshold (the port's `max_property_bytes`: the server's maximum request
  size, with BIG-REQUESTS if it has it, less the request header; capped at 256 KiB) is written in
  one `ChangeProperty`, then notified. Above it, INCR:
  1. select `PropertyChangeMask | StructureNotifyMask` on the requestor;
  2. write type `INCR` with the total size, and notify;
  3. on each `PropertyNotify(Delete)` of that property, write the next chunk;
  4. after the last chunk, write a zero-length chunk;
  5. on its delete, the transfer is done; clear the event selection unless another transfer to
     that requestor is live.
  Each transfer is keyed by (requestor, property). A new request for the same pair cancels the
  old transfer. Each has a 10 s inactivity deadline, and is dropped on the requestor's
  `DestroyNotify`. A slow or dead requestor stalls only its own transfer.
  Motif's `CLIP_TEMPORARY` requests never get INCR, which Motif cannot read
  (qxcbclipboard.cpp:443-446); a reply too large for one property is refused.
- `SelectionClear` for a selection we own means we have lost it. The state becomes **Theirs**,
  and the XFixes notification that follows starts a capture.

## Commands from the daemon

- **Serve(content)**: the user re-selected a history entry. Claim with a fresh server timestamp,
  then verify.
- **Restore(content)**: the daemon starts with a saved history. Only if the engine believes
  CLIPBOARD unowned: grab the server, take a server timestamp, check `GetSelectionOwner`, claim
  with that timestamp if there is no owner, ungrab, and verify. Under the grab no client can claim
  between the check and our claim. A client claiming after the ungrab at `CurrentTime`, or with a
  time later than ours, wins. One remaining loss, which X offers no way to close: a client whose
  claim carries an event time *earlier* than ours (a key press before the restore) but reaches the
  server after it is refused by the server's time rule; an ICCCM client sees that its claim failed.
  `ClaimFailed` if anything owns it.
- **Clear**:
  - abandon any capture in flight, so a slow capture cannot re-add what was just cleared;
  - end the keep-alive candidate;
  - claim CLIPBOARD as **Ours(cleared)** with a fresh server timestamp, and PRIMARY with the same
    timestamp, also offering only `TARGETS` and `TIMESTAMP` (a selected password is as visible to
    a middle-click as a copied one). PRIMARY's claim is not reported; losing it (`SelectionClear`
    not older than the claim) just ends it. Unless a clear holds PRIMARY, a PRIMARY request is
    refused; PRIMARY is never served CLIPBOARD's content;
  - empty the root window's `CUT_BUFFER0` … `CUT_BUFFER7`. xterm and urxvt fall back to the cut
    buffer when a selection paste is refused, and xterm writes `CUT_BUFFER0` on every selection.

## Liveness

Plan §3's failure 3, "nothing copies from any app", is the one a clipboard tool must never cause.

- **The engine cannot wedge.** Every wait has a deadline, every transfer is independent, and a
  property in doubt is quarantined rather than reused.
- **Self-check.** Every 30 s while the selection is **Theirs** and no capture is running, the
  engine converts `TARGETS` (a cheap read every owner must answer). Two unanswered checks in a
  row mean the owner is alive but hung. The engine reports that to the daemon, which shows it in
  the tray. It does not take the selection from a live application.
- **The adapter's loop has a watchdog.** If the loop has not turned for 5 s, the daemon exits, and
  systemd restarts it (plan §5.6).

## What the adapter guarantees

- A property read returns the whole property, or reports it as too large without returning it.
  A read with delete deletes only after the whole property has been read; X deletes on
  `GetProperty` only when nothing remains after the read.
- X errors from requests to windows that have gone (a requestor destroyed mid-transfer) are
  logged and otherwise ignored. The engine's deadlines and `DestroyNotify` clean up.
- Events reach the engine in server order.

## Red team

### Pass 1, 2026-10-24

A separately briefed reviewer read GTK 3/4, Qt, the X server, Chromium, Emacs and clipman. Two of
its most consequential claims were checked in the X server and Qt source before revision.

| # | Finding | Resolution |
|---|---|---|
| 1 | `SAVE_TARGETS` from an app that owns only PRIMARY would make us steal CLIPBOARD from a live app (Qt does send it then; checked) | Act only if the requestor owns CLIPBOARD; else reply success, take nothing |
| 2 | "Everything you can" converts targets with side effects (`DELETE` clears Emacs's selection) | Never-convert list on every path |
| 3 | One fixed property per selection lets late replies and INCR chunks from abandoned transfers corrupt newer captures | Property pool, replies matched on (selection, target, property, time), 60 s quarantine |
| 4 | The keep-alive candidate goes stale: copy-and-quit with nothing kept serves the previous copy | Candidate tagged with its selection time; ended by any owner change. (Death notifications carry owner None, found while implementing, so the tag cannot include the owner window.) |
| 5 | Claiming with the event timestamp can overwrite a newer owner; the server does not touch `lastTimeChanged` on death (checked: dix/selection.c:217, 181) | Claim with `selectionTimestamp` |
| 6 | `SAVE_TARGETS` with property None; GTK 4 treats a None reply as failure | None means all; reply with a non-None property |
| 7 | The `SAVE_TARGETS` budget exceeded what apps wait (GTK 3 10 s, Qt 5 s) | 4 s budget; reuse and extend the in-flight capture |
| 8 | Clear leaks through captures in flight and through cut buffers | Clear abandons captures and empties `CUT_BUFFER0-7` |
| 9 | INCR receive: delete-on-read only when fully read; our own Delete notifications; trickling owners | Spelled out; 60 s capture deadline |
| 10 | Serving: `TEXT` typing, missing `text/plain` and `COMPOUND_TEXT`, no image aliases, same-pair INCR, Motif | All specified; image aliases via off-engine transcoding |
| 11 | The plan's self-check was missing | Liveness section |
| 12 | A 2 s answer deadline is too short for renderers | 10 s, text first, progressive publish |
| 13 | The canonical set dropped RTF, SVG, `text/x-moz-url`, Chromium web data | Fetch all `text/*`, vector images, and those named formats |
| 14 | `TARGETS` parsing: None, duplicates, type `TARGETS`; no image fallback | Specified |
| 15 | Owner windows rarely carry `WM_CLASS` | Attribution chain |

### Pass 2 (code review), 2026-10-24

A separately briefed reviewer read the implementation against this document. Each finding below
has a test that fails without its fix (checked by reverting each fix in a copy of the tree).

| # | Finding | Resolution |
|---|---|---|
| 1 | Replies not matched on time; a late refusal from an older conversion was taken as the new owner's, dropping it to text-only and releasing a property still in use | Refusals must match the conversion's time exactly ("Properties for incoming transfers") |
| 2 | Stale `SetSelectionOwner`, `SelectionClear` and death events, queued before our claim, made the engine believe it no longer owned CLIPBOARD, so every paste was refused | "Events judged by time" |
| 3 | An outstanding probe's property leaked on ownership changes; with a hung owner the pool would run dry and capture would stop, silently | Probe stopped and its property quarantined on every transition |
| 4 | `SAVE_TARGETS` before the `TARGETS` reply lost what it asked for (copy-and-quit) | Kept and merged after `TARGETS` (step 0) |
| 5 | A pending `SAVE_TARGETS` left unanswered or wrongly answered on transitions | "Every transition settles what is pending" |
| 6 | A `TARGETS` reply of type `INCR` released a property its owner was still writing | Treated as refusal; property quarantined |
| 7 | The owner present at start can never be kept alive | Documented (Keep-alive) |
| 8 | A small reply replacing an INCR transfer left its requestor watched | Unwatched |
| — | Divergences: state after `SelectionClear`; `MULTIPLE` offered when cleared; non-INCR `TooLarge` released, not quarantined; INCR threshold hard-coded at 256 KiB (exceeds the request limit without BIG-REQUESTS: `BadLength`, lost paste); no end report for a capture cut short | Document and code aligned; threshold from the port; `CaptureEnded` |
| — | The port's unstated contract (byte order, inclusive limits, event queueing during queries, `server_time`'s property, our own window's mask) | Written in `Port`'s documentation |
