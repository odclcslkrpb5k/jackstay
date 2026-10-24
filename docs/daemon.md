# The daemon (M1)

How `jackstayd` runs the clipboard manager: threads, history and its storage, clearing, the
control socket and the `jackstay` CLI, clipman import, and how it starts and stops. Plan §5.2 and
§5.6 say what it must do; this says how. M1 has no network and no UI; both attach later through
what is described here.

Status: **design, revised after red-team pass 1** (findings at the end). clipman references are to
xfce4-clipman-plugin 1.6.6.

## Threads

- **Clipboard thread.** Owns the X connection (`jackstay_x11_clip::Clipboard`), the history
  (`jackstay_clip::History`) and the control clients. One `poll` waits on the X fd, the control
  socket, the client connections, the store thread's reply pipe and the signal pipe, for the
  clipboard's timeout but **never longer than 1 s**, so the loop always turns. Nothing in it waits
  on disk or on a client: client sockets are non-blocking (below), and the disk is the store
  thread's.
- **Store thread.** Owns the files and their reference counts (below). Failures are reported back;
  a history that cannot be saved stays usable in memory and is reported (`jackstay status`, the
  journal). If the store thread dies, the daemon exits 101.
- **Main thread.** Parses configuration and flags, takes the instance lock, checks coexistence,
  starts the two threads, and waits for either to end. A panic in either thread ends the process
  with 101 (a thread's panic does not, by itself, under `panic = "unwind"`).

## Liveness

`Type=notify` with `WatchdogSec=30`: each loop turn sends `WATCHDOG=1` to `$NOTIFY_SOCKET` (a std
`UnixDatagram`). The 1 s poll cap keeps an idle daemon turning. A turn can block only inside an X
round trip; a client holding a server grab for more than 30 s gets us restarted, which is
acceptable. `WatchdogSignal=SIGKILL` and `LimitCORE=0`: a core dump would hold clipboard contents.

## History in the daemon

- **Current entry.** The daemon tracks which history entry, if any, is on the clipboard now:
  - the entry of a capture, once its first content arrives (`Captured`);
  - the entry served after `select`, a keep-alive, or a restore at start;
  - nothing after an owner change whose capture has not reported yet (`CaptureStarted`), after
    `CaptureFailed`, after `Lost`, or after a clear.
- **Progressive captures.** The first report of a capture adds a **new** entry
  (`History::record_partial`, or `History::record` if it is already complete); later reports of
  the same capture replace that entry's content, keeping its id, pin and place
  (`History::update`). Deduplication against the rest of history happens only on the `complete`
  report: if the complete content equals an older entry's, the two merge into the older one's id,
  in the new entry's place, pinned if either was. A partial report never rewrites an older entry.
  The limits apply and evictions are reported on every update.
  - The id `update` returns is the entry's from then on: after a merge it is the older entry's,
    and both the capture's entry and the current entry become it.
  - `evicted` may hold that id itself (a large partial pushed back by a promote, then over the
    byte budget), and `update` returns `None` for an entry already gone; either way the capture
    has no entry any more, its later reports are ignored, and the current entry is none.
- `CaptureEnded` and `CaptureFailed`: the entry, if any, stands as last reported, and is completed
  (`History::update(id, last content, true)`), so a capture cut short is deduplicated like any
  other. Every capture is thereby completed before the next one starts (`CaptureEnded` precedes
  `CaptureStarted`), so duplicates exist only while one capture is open.
- Every eviction and deletion goes to the store (below).
- **Re-select** (`jackstay select <id>`): `History::promote`, then `Clipboard::serve`.

## Clearing

- **`jackstay clear`**: `Clipboard::clear()`, which claims CLIPBOARD **and PRIMARY** as cleared
  (offering nothing) and empties the cut buffers; then the current
  entry, if any and **not pinned**, is deleted from history and disk.
- **`jackstay delete <id>`**: deletes the entry, pinned or not; if it is the current entry, the
  clipboard is cleared as above.
- **`jackstay clear-history`**: every unpinned entry deleted; the clipboard cleared; also every
  leftover the store keeps (`index.broken-*`, `index.tmp`, moved-aside blobs).
- A clear is answered only after the store confirms the files are gone.
- Deleting is `unlink`; overwriting first would not reliably destroy data on an SSD with a
  journalling file system. Full-disk encryption is the control for data at rest; the README says
  so, and says that backups of `$XDG_STATE_HOME` keep what they copied.

## Storage

Under `$XDG_STATE_HOME/jackstay/` (default `~/.local/state/jackstay/`; a relative
`$XDG_STATE_HOME` is ignored, per the XDG spec). History is state: worth keeping across restarts,
not a document; `~/.local/state` is less often backed up than `~/.local/share`.

- The directory is created `0700`; an existing one is checked with `lstat` and its mode fixed;
  if it is a symbolic link, not a directory, or another user's, the daemon refuses (exit 2). Its
  own `lock` (`flock`) keeps a second daemon with another runtime directory from sharing the
  history (exit 3). A history that cannot be read refuses the start (exit 2): running without one
  would let a clear made now be undone by the next start that reads the old one. Files are created with `O_NOFOLLOW |
  O_EXCL` (temporary names) and mode `0600`, then renamed.
- `blobs/<sha256 hex>`: one file per distinct kept target's bytes.
- `index`: a version line, the next entry id, a `cleared` flag, then one record per entry, newest
  first: id, copied-at, pinned, source, and per target its name and either `kept <type> <format>
  <sha256>` or `not-kept <reason>`, then an `end` line. Every field percent-escaped in one
  canonical spelling; parsed with no-panic code and fuzzed (BD-1, TS-16). The format, with an
  example, is defined in `crates/store/src/index.rs`. Reading raises `next-id` above every id read,
  and says whether the file ended where a written index ends.
- **Writing** (store thread, `crates/jackstayd/src/store.rs`). The clipboard thread sends the
  store a *snapshot*: the whole current history (cheap: entries share their content through `Arc`)
  and the `cleared` flag, marked *urgent* for clears and deletions. The store coalesces: if several
  snapshots are waiting it writes only the latest.
  - An urgent snapshot first unlinks the blobs no longer referenced and `fsync`s `blobs/`, so the
    bytes are gone, durably, even if a write then fails (a full disk); then writes the missing
    blobs, then the index.
  - An ordinary one writes the missing blobs, then the index, then unlinks (and `fsync`s), so the
    old index never names a missing blob.
  - Each blob is written to `.tmp-<hash>` (`O_EXCL | O_NOFOLLOW`, `0600`), `fsync`ed and renamed;
    `blobs/` is `fsync`ed before the index names it. The index is written to `index.tmp`,
    `fsync`ed, renamed, and the directory `fsync`ed.
  - Ordinary snapshots are written at most once a second; urgent ones at once, and acknowledged
    once on disk. A failed save is reported, kept, and retried every second (as urgent) until it
    succeeds or a newer snapshot replaces it; the latest is written at stop.
- **Reading at start.** Records are parsed one by one: a bad record is dropped and logged (by
  number, never content) and the rest kept. If the header itself is unreadable, the index *and*
  `blobs/` are moved aside together (`*.broken-<time>`) for recovery, nothing is deleted, and
  history starts empty: a corrupt store never stops the clipboard (plan §5.2). Entries whose blob is
  missing are dropped; if the newest one is, the history counts as cleared (a clear may have
  deleted its blob without reaching the index, and nothing older may take its place on the
  clipboard). Blob hashes are verified on the store thread after start, a few between requests,
  never before serving; a mismatch drops that entry. After a clean read (nothing dropped, the file
  whole), blobs no entry names (written just before a crash) and interrupted writes are deleted;
  after any other read they are kept for recovery until the history is cleared.
- **Restart restore.** If the index's `cleared` flag is set, nothing is served. Otherwise the newest
  entry is served **only if CLIPBOARD has no owner** (`Clipboard::restore`: the check and the
  claim happen under a server grab, so an application claiming meanwhile wins; docs/selection.md,
  "Commands from the daemon").

## Control socket

`$XDG_RUNTIME_DIR/jackstay/control`, a Unix stream socket in a `0700` directory; if
`$XDG_RUNTIME_DIR` is unset the daemon refuses (exit 2) rather than fall back to `/tmp`. Beside
it, `lock`: the daemon takes an exclusive `flock` before it creates anything (flags and the
configuration are read first, so a bad one refuses without touching anything); a second instance
exits (status 3) and says another jackstayd runs. A stale socket is removed only while holding the
lock.

Each connection's peer is checked with `SO_PEERCRED`: another uid is refused before reading.
Sockets are non-blocking, and what one client can cost the clipboard thread is bounded
(`crates/jackstayd/src/control.rs`): at most 16 clients; a request frame of at most 16 bytes,
refused from its header; a client is read only while it has under 64 KiB buffered and under 1 MiB of
responses unread, and while it is not waiting for a deferred response; at most 64 of its requests
per turn of the loop; one with 4 MiB unread is disconnected; one that has not said `Hello` within
10 s is disconnected. A partial frame does not count as work, so a client that stalls mid-frame
lets the loop sleep. A malformed frame disconnects. A client that closes its end for writing still
has what it sent handled and answered. When accepting fails (out of descriptors), accepting pauses
for a second rather than spin.

Frames as in `proto` (length, kind, payload) with the same no-panic decoding, fuzzed, in a message
namespace of its own with its own version (`control`), separate from the peer protocol. Requests:
`Status`, `List { limit }` (ids, times, sources, kinds, sizes, a text preview of at most 200
characters), `Select { id }`, `Delete { id }`, `Pin { id, pinned }`, `Clear`, `ClearHistory`,
`Subscribe` (history changes, for the UI later). Responses carry a result or an error code (ER-8).
A clear answers `ClaimFailed` if another application held the clipboard, so it was not cleared.
Sources and previews are cleaned of control, format and separator characters, in the daemon and
again in the CLI, so nothing a client sets (its `WM_CLASS`) reaches a terminal as an escape.

`jackstay` is the thin CLI over it: `status`, `list`, `select`, `delete`, `pin`, `unpin`, `clear`,
`clear-history`, `import-clipman`.

## clipman import

clipman 1.6.6 keeps its history in `$XDG_CACHE_HOME/xfce4/clipman/` (`g_get_user_cache_dir()`,
plugin.c:183, 196):

- `textsrc`: a GKeyFile, group `[texts]`, key `texts`, a string list (plugin.c:279): `;`-separated,
  with GKeyFile escapes (`\;`, `\n`, `\s`, `\t`, `\r`, `\\`), UTF-8, **oldest first** (saving
  reverses the newest-first history, plugin.c:249).
- `image<N>.png`: PNGs, oldest first (plugin.c:263). clipman deletes each as it loads it at start
  (plugin.c:186), and every save rewrites the directory (plugin.c:226-238), so images exist only
  if something was copied since clipman last started. Nothing is saved at all if clipman's
  save-on-quit is off (plugin.c:241-243).

The file is read as GLib reads it (`crates/store/src/clipman.rs`): one clipman would refuse to
load imports nothing, and empty items are skipped as clipman skips them.

Import, on first start (no `imported-clipman` marker) and on `jackstay import-clipman`: each item is
added at the **back** of history (older than anything Jackstay has), texts newest first and then
images newest first, so each keeps clipman's order; content already present is skipped, and the
file's modification time is the copy time. At first start the import comes after the restore, so
nothing imported is put back on the clipboard; the marker is written only once the imported history
is on disk. The import runs on the clipboard thread (a few small files, read once, on request). clipman's own files are left alone, but the command says plainly that they still
hold that history, and `jackstay import-clipman --remove` deletes them (pre-migration secrets would
otherwise survive every Jackstay clear). (`History::append_oldest`: a full history evicts the
import itself.)

## Coexistence (plan §5.6)

At start, after the lock:

- `CLIPBOARD_MANAGER` already owned (xfce4-clipman's daemon owns it with `CurrentTime` and never
  answers): exit 3. X releases a selection when its owner
  disconnects, so a dead clipman cannot leave it owned.
- A process of this user that is clipman (`/proc/*/comm` exactly `xfce4-clipman`, or a command
  line naming the panel plugin, `xfce4-clipman-plugin` or `libclipman.so`, as the panel's
  `wrapper-2.0` is given it), or the plugin in the panel's plugin list (`xfconf-query -c
  xfce4-panel -p /plugins -lv`, read on a thread, 2 s timeout, a `/plugins/plugin-N` whose value is
  `xfce4-clipman-plugin`): exit 3. These names are from xfce4-panel's and clipman's sources and
  are to be confirmed on a live Xubuntu.
- While running, the process check repeats every 30 s; clipman starting after us also exits us
  (3), with the reason in the journal, since two managers fight over ownership.
- xfsettingsd 4.20 has no clipboard manager, so it is no conflict.

## Starting and stopping

- A systemd user unit, `jackstayd.service` (`data/systemd/`), `Type=notify`, `Restart=on-failure`,
  `RestartSec=2`, `RestartPreventExitStatus=0 2 3 4`, at most 10 starts a minute, no `WantedBy=` (it
  is started per session, below). xfce4-session does not start `graphical-session.target`, so the
  daemon ends with the session through the X server going away (status 4). No
  `ProtectSystem=` or other sandboxing that needs user namespaces (restricted by AppArmor on
  Ubuntu) and no `PrivateNetwork=` (it would cut the abstract X socket).
- Started from XDG autostart by `jackstay-session`, which runs `systemctl --user start
  jackstayd.service`. xfce4-session's xinitrc already exports `DISPLAY` and `XAUTHORITY` to systemd
  (xinitrc.in:86-95); the script imports them again anyway, which is harmless.
- `SIGTERM` (through a signal pipe, `signal-hook`): stop taking requests, let the store write the
  latest snapshot, exit 0; a save that fails then is logged.
- Flags for running a second instance beside the live one: `--display`, `--state-dir`,
  `--runtime-dir`.

## Configuration

`~/.config/jackstay/config.toml`, every key optional:

| Key | Default | Meaning |
|---|---|---|
| `history.max_entries` | 100 | unpinned entries (clipman's default) |
| `history.max_bytes` | 512 MiB | unpinned bytes, newest excluded |
| `history.max_pinned` | 1000 | pinned entries; pinning beyond is refused |
| `capture.per_target` | 64 MiB | above a raw 4K screenshot (33 MB) |
| `capture.per_entry` | 128 MiB | |

Parsed once at start into a typed config; an unknown key or a bad value refuses (exit 2) with the
key named in the journal. The whole history is held in memory, within these limits.

## Exit statuses

| Status | Meaning | Restart |
|---|---|---|
| 0 | Stopped by request | no |
| 2 | Configuration, paths or state directory invalid, or the history unreadable | no |
| 3 | Another clipboard manager, or another jackstayd, is running | no |
| 4 | X connection lost after running (logout) | no |
| 5 | X not reachable at start (login race) | yes |
| 101 | Panic | yes |

## Logging

To the journal through stderr, one line per event: kinds, codes and counts, never clipboard
contents, previews, sources' window titles, or file names from the history (DP-3). `Content`,
`Kept` and `Report` print sizes, not bytes, through their `Debug`, as do the engine's own
buffers (a read property, an INCR transfer either way, the served text), so no `{:?}` or panic
message can leak contents; `dbg!` is denied workspace wide (clippy `dbg_macro`, proven by a plant).

## Dependencies this adds

Each to be justified in `docs/dependencies.md`: `sha2` (RustCrypto, pure Rust) for blob names;
`toml` with `serde` for configuration; `signal-hook` for the signal pipe.

## Red team

### Pass 1, 2026-10-24

A separately briefed reviewer, reading plan.md, selection.md, the code, clipman 1.6.6,
xfce4-session 4.20 and xfce4-settings 4.20.

| # | Finding | Resolution |
|---|---|---|
| 1 | The watchdog would kill an idle daemon: the poll sleeps with no timeout when nothing is pending | 1 s poll cap; systemd `WatchdogSec=30`; no core dumps |
| 2 | A corrupt index would have deleted every blob, pinned ones included | Per-record salvage; an unreadable header moves index and blobs aside; no deletion without a clean read |
| 3 | A restart could steal the clipboard from a live application, or un-clear a cleared one | Restore only if unowned; persisted `cleared` flag |
| 4 | "Current entry = newest" is wrong during slow or failed captures, and would delete pinned entries | Tracked current entry; `CaptureStarted` report; clear never deletes a pinned entry |
| 5 | PRIMARY was not cleared | Clear claims PRIMARY too |
| 6 | clipman's format was guessed; it is a GKeyFile, oldest first, under `$XDG_CACHE_HOME`; images rarely exist on disk | Import section rewritten from the source; imports go to the back; `--remove` |
| 7 | A partial capture could rewrite an older (pinned) entry through dedupe | Dedupe only on completion; partial reports update only their own entry |
| 8 | Client sockets could block the clipboard thread | Non-blocking, bounded frames and output, client cap |
| 9 | Store ordering, refcount races, unbounded queue, unnoticed store death | Snapshots, coalesced; store owns refcounts; ordering per urgency; death ends the daemon |
| 10 | `Debug` of content and reports prints the bytes; a thread's panic does not end the process | Redacting `Debug`; `dbg!` denied; join errors exit 101 |
| 11 | A second jackstayd would call itself "another manager"; clipman starting later was only reported | `flock`; periodic check exits |
| 12 | systemd details: `WantedBy`, a transient refusal at start never retried, sandboxing needs user namespaces | Unit and statuses revised |
| 13 | Paths: unset runtime dir, relative XDG variables, symlinks, permissions, backups | Paths section |
| 14 | No configuration, no pinned bound, no test-instance flags, new dependencies unjustified | Configuration, flags, dependencies sections |
