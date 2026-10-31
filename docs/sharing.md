# Clipboard sharing

How a copy on one machine becomes the clipboard on the others (milestone M5). Plan §5.4 says what
clipboard sharing must do; this says how, and where it departs from the plan (marked
**Departs**, each the owner's decision of 2026-10-31 or a red-team resolution). It rides on the
link (docs/link.md) and on the selection engine (docs/selection.md).

Status: **design, revised after red-team pass 1** (findings and resolutions at the end). Nothing
built. Code facts are as read on 2026-10-31 at 2c9e386.

## What exists to build on

- `proto::announce` (`Announce`, `EntryId`, `Target`, `TargetName`) and `proto::frame` are
  scaffolding from M0 that nothing uses. Their `TargetName` is printable ASCII up to 255 bytes;
  `clip`'s is Latin-1 up to 4096 (plan §6 moves the adoption here). Both go; the messages below
  replace them inside `proto::link`, the codec the link already speaks.
- `net` speaks on stream 0 only: one `open_bi`/`accept_bi` per session, nothing else accepted,
  and `Sender::send` passes only input kinds. A stalled write in a session's loop stalls the
  session (`session.rs`, `live`). quinn's transport defaults stand (`send_window` 10 MB, stream
  window 1.25 MB, 100 streams each way, CUBIC).
- `selection` answers every request at once. The deferred reply docs/selection.md describes for
  transcoded image aliases was never built; the only deferred `SelectionNotify` is
  `SAVE_TARGETS`'s.
- `clip::Content::new` needs a kept target; `same_kept` compares kept targets only.
  `clip::Source` is `Local { owner_class }` only; the store's index accepts only
  `source local [class]` and drops any other record. History ids are per machine, from 1.
- Blobs on disk are named by their SHA-256 (`store`, `blob.rs`).

## Shape

- **Departs (§5.4): the clipboard follows the newest change at once, not on the pointer's
  arrival.** A machine that receives a change newer than its own clipboard takes CLIPBOARD
  straight away, pointer or not. On arrival needed a hand-off on every way a span ends, raced a
  copy made just before crossing, and shared nothing with input sharing off. Synergy, whose
  fight over ownership was §5.4's reason, is gone. This adds an exception to §5.2's "while an
  owner lives, we never take ownership": a newer change from a peer takes it, except from a copy
  still being captured here (Order).
- **Departs (§5.4): every received entry is fetched whole in the background,** within the
  history's limits, so history on each machine is whole even when the other is asleep or off.
  Targets up to 64 KiB travel inside the announcement; the rest are fetched one at a time.
- **An entry is identified everywhere by its digest:** SHA-256 over its targets' names, types,
  formats and each target's own SHA-256, in name order. Two machines holding the same copy agree
  on it without any mapping of history ids, and a history merge on one machine changes nothing
  for the others.
- **Each change is announced to every connected peer** and ordered the same way everywhere
  (Order). A change is a copy captured, a re-select, or the clipboard emptied. Clears and
  deletions travel too, and are remembered for a peer that is away (Forget).
- **Two machines.** Messages go to each connected peer and are not relayed. With three or more,
  a machine that was away while the third was off can lag until the next change; M5 is accepted
  on two, and says so.

## Order

- Each change carries a **tick** from a hybrid clock: `max(wall-clock ms, last tick + 1)`, where
  the last tick counts every tick this machine has used or received. A tick is taken when the
  change **starts**: for a copy, at the owner change (`CaptureStarted`), not when content
  arrives. Changes order by `(tick, origin key)`, the same everywhere, so two machines end on the
  same clipboard.
- Wall-clock based, so after the machines lose touch (a Wi-Fi drop, one asleep), the copy made
  later wins on reconnect, which is the one the user means; NTP keeps the two within
  milliseconds. A received tick more than a day ahead of this machine's wall clock is refused
  (the message is dropped and the journal says so), so one wrong clock cannot pin every machine
  to it. The last tick is saved with the history; after a restart, wall-clock time has moved on
  past any tick used, unless a peer's clock ran ahead of ours, in which case a tick can repeat
  and the key orders it.
- A machine's **clipboard state** is its newest applied change: an entry (digest) or empty. On
  a received change newer than the state, a machine applies it, **unless a capture here is in
  flight** (a live owner still answering): then it waits for that capture to end, and applies the
  received change only if it is still the newer. A copy here is never abandoned for one from a
  peer.
- **No loop with other clipboard tools.** A capture whose content's digest equals the clipboard
  state's is not a change: our own serve recaptured by another tool that copies whatever owns
  the clipboard (CopyQ, spice-vdagent, VirtualBox's guest additions) is not announced again.
  Beyond that, more than 10 changes a second from captures stops announcing captures for 10 s,
  and the journal and `jackstay status` say so.

## Messages

`proto::link` kinds, on their own streams (Streams). The link protocol version becomes 4.

| Kind | Way | Payload |
|---|---|---|
| `Announce` | any → peer | tick (u64), digest (32), replaces (flag + digest), copied-at (u64 ms), count (u8 ≥ 1), each target: name (u16 length ≤ 4096, Latin-1), type name (same), format (u8: 8, 16 or 32), size (u64), SHA-256 (32), inline flag, and if inline its bytes |
| `Emptied` | any → peer | tick: the clipboard was cleared, or its owner gave it up |
| `Forget` | any → peer | tick, then 1 to 1000 digests: delete these entries everywhere |
| `Fetch` | peer → announcer | digest, target name |
| `Data` | announcer → peer | the bytes, the stream's rest; or `Gone` (not held) |

- **Inline.** An announcement carries targets of up to 64 KiB each, in the capture's order (text
  first), until 512 KiB in all. The frame's limit is 1 MiB.
- Decoding refuses a name or type outside Latin-1 or past 4096 bytes, a format other than 8, 16
  or 32, an inline length that differs from its size, a duplicate name, and a digest that does
  not match the targets. The receiver refuses a target past its per-target limit as `TooLarge`.
- `Data` has no size of its own: the fetcher knows it from the announcement, reads exactly that
  many bytes into a buffer of that size, checks the SHA-256, and treats a stream that ends short,
  runs long, or does not hash as failed.

## Streams

- Each `Announce`, `Emptied` and `Forget` goes on a **unidirectional stream** of its own,
  finished after the message. Each `Fetch` opens a **bidirectional stream**, answered on it. A
  fetcher abandoning a fetch stops the stream (`STOP_SENDING`); the announcer resets it when the
  entry goes.
- Order across streams is not kept, and every message is safe out of order: changes carry ticks,
  a `Forget` is remembered (Forget) so an `Announce` arriving after it is refused, and an
  `Announce` for a digest already held only adds what is missing.
- **Input comes first** (plan target 2 over the bulk of target 1).
  - One fetch at a time per peer, and at most 256 KiB of clipboard bytes unacknowledged per
    connection: the receiver sets clipboard streams' receive window to 256 KiB, and the sender
    writes a fetch in 64 KiB pieces. On a 100 Mbit/s link a full window is 20 ms of queue that
    stream 0 can wait behind; on gigabit, 2 ms. quinn's stream priority (stream 0 highest) only
    orders what quinn still holds, and is kept as well.
  - At most 16 clipboard streams open each way per session (quinn's stream limits, plus stream
    0); messages beyond wait for a slot. At most 64 announcements are held unprocessed; beyond
    that the oldest are dropped, and the journal says so.
  - Each clipboard stream runs in a task of its own; a stalled one never holds a session's loop.
- **Through the daemon.** The link thread hands each message to the clipboard thread on the
  channel its events use (`net::Event::Clip`), fetched bytes whole as `Arc<[u8]>` (one 64 MiB
  buffer at most, one fetch at a time); the clipboard thread asks the link to announce, empty,
  forget and fetch (`Handle::clip`). This replaces docs/link.md's "never copying a whole target on
  the link thread": the copy is the one buffer, handed over.

## Capture and announce

- An entry is announced when its capture **completes**, or 1 s after its first content if it has
  not, so text never waits long for a slow image. A capture announced early is announced again
  when it completes, with the same tick and `replaces` naming the early digest; a receiver drops
  the early entry if it came from that announcement and is not pinned there.
- **A re-select** (`jackstay select`) is a change: the entry is announced again with a new tick,
  whatever its origin. Receivers that hold its digest move it to the front and serve it; others
  add it and fetch what they lack from the announcer. **Departs (§5.4),** which never
  re-announced received entries: the digest and the no-loop rule above make it safe, and it needs
  no "please announce again" message.
- If completion merges the capture into an older entry here (docs/daemon.md, "Progressive
  captures"), nothing more is sent: the digest is the same.
- A keep-alive (our claim after the owner died) and a restore at start are not changes.
- **An owner giving the clipboard up** (XFixes owner None, as password managers do to clear) is a
  change: `Emptied`. History keeps the entry here, as before (no filtering, plan §5.2).

## Receiving

- `Announce` adds the entry to history (`Source::Peer { name, key }`, the copied-at time sent),
  **placed by its tick** among the entries, not at the front, so an older change never displaces
  the current one in the popup. An entry whose digest is already held gains the peer's source if
  it had none, and nothing else.
- Targets past 64 KiB are **pending** (`clip::TargetData::Remote { size, sha256 }`), and count
  as held: an entry of a screenshot alone is an entry. A pending target whose blob is already in
  the store (the same bytes copied before) is filled from it, not fetched.
- **Fetching:** one at a time per peer, the clipboard's own entry first, smallest target first; a
  paste waiting (Paste) preempts any other fetch, which goes back to the queue. A fetch ends
  `Gone` or failed: the target becomes `NotKept { OwnerFailed }`; while the entry is the
  clipboard, the next session retries it. An entry evicted or deleted while being fetched
  cancels its fetch.
- Received bytes count against the history's limits as local ones do (512 MiB, 64 MiB a target):
  copies from a peer can evict older local entries, as more local copies would.
- If the change is newer than the clipboard state, the clipboard thread serves the entry, pending
  targets offered. A later `Announce` of the same change (complete after early) updates what is
  served **in place** (a new `Clipboard::update`), without claiming again, so applications see no
  spurious `SelectionClear`.

## Paste

A request for a pending target **waits**:

- The engine defers that request's `SelectionNotify` (a `Deferred` state per request, as
  `SAVE_TARGETS` already has) and reports `Wanted { target }`; the fetch goes to the front.
- The reply goes out when the bytes are whole, served as any held target (INCR above the
  threshold). **Never streamed:** once INCR has started, a zero-length chunk is the only end,
  which the requestor reads as complete, so a dropped link would truncate the paste silently
  (§5.4).
- **Deadline 3 s,** then refused (`property None`) before any byte is sent. Requestors wait:
  Firefox **1 s** (`kClipboardTimeout`, widget/gtk/nsClipboard.cpp), Qt 5 s, Chromium 10 s
  (`kRequestTimeoutMs`, ui/base/x/selection_requestor.cc), GTK 3 30 s (`IDLE_ABORT_TIME`,
  gtkselection.c). Firefox therefore gives up on a pending target that takes more than 1 s; the
  background fetch, started at the announcement, is what makes that rare. A MULTIPLE waits for
  all of its pending targets within the same deadline. The clipboard moving on while a request
  waits refuses it.
- A paste on the announcer never waits: it holds what it announced.

## Forget

- **Departs (§5.2) in part, the owner's decision:** `jackstay delete` deletes the entry on every
  machine, **pinned or not** (a password deleted on one machine is not still pasteable on
  another). `jackstay clear` empties the clipboard everywhere (`Emptied`) and forgets the current
  entry everywhere; where it is pinned it stays, as `jackstay clear` spares a pinned entry here
  (docs/daemon.md, "Clearing"). `jackstay clear-history` stays on this machine, besides the clear
  it includes; §5.2 had every removal propagate.
- A machine **remembers what it forgot**: the last 1000 `(digest, tick)` deleted, from here or a
  peer, persisted with the history. An `Announce` of a remembered digest with a tick no newer
  than the forget's is refused, so a delete that overtakes its announcement still wins; the same
  content copied again later has a newer tick and is kept.
- **A peer that is away** gets the remembered forgets, all of them, in one `Forget` per 1000, on
  its next session; deleting what is not held is nothing, so no acknowledgement is needed. A
  deletion older than the last 1000 can be missed by a machine away for that long. Each side also
  sends its clipboard state on a new session (`Announce` or `Emptied` at its original tick); the
  older side applies it.
- **Unpairing** keeps the peer's entries in history (they are history) and forgets nothing.

## In history, the store and the interfaces

- `clip`: `Source::Peer { name, key }`; `TargetData::Remote`; the digest; placing by tick
  (`History::record_at`).
- **The store's index becomes version 2:** `source peer <fingerprint> <name>` and `remote`
  target lines with their hashes, the last tick in the header, and the remembered forgets. A
  version-1 index is read and rewritten as 2. An older Jackstay finds version 2 unknown and, as
  for any index it cannot read, moves it aside and starts empty (docs/daemon.md, "Storage"):
  downgrading loses history, which the README says.
- **Control version 6:** an entry's summary says which peer it came from (its name, printable, ≤
  64 bytes); the popup shows "from xiphos" where it shows the copying application now.
- **README:** copies, passwords included, are kept on every paired machine's disk; deleting one
  deletes it everywhere.
- The journal says when an entry is announced, received and fetched, by digest prefix, with
  sizes, never contents (DP-3).

## Testing

- `proto`: each kind's limits and refusals, round trips (property tests), digests that do not
  match refused, the fuzz target extended and its corpus regrown.
- `net`, through the relay, which gains a **bandwidth limit with a queue** (a 100 Mbit/s link
  with 50 ms of buffer): an announcement and a fetch; a 32 MiB fetch under loss and delay; the
  link dropped mid-fetch (failure, nothing partial); a peer sending too many streams, an
  announcement flood, and `Data` too long, too short or not matching its hash; and **motion
  latency during a 32 MiB fetch on the limited link**, against the input latency gate.
- `selection`: deferred replies through the fake port: answered when the data lands, refused at
  the deadline, MULTIPLE waiting, the requestor dying while it waits, the clipboard moving on
  while it waits.
- `clip` and `store`: digests, `Remote` targets, placing by tick, the forgets, the index's
  version 2 and its upgrade from 1, fuzzed as now.
- Two daemons (the relay rig, `xclip` as the applications):
  - text copied on A pastes on B, and back; an image-only entry pastes whole on B;
  - a 16 MiB image pastes on B while its fetch is still running (delay fault), and is refused,
    not truncated, when the link drops mid-way; a requestor with a 1 s timeout;
  - a change from A while B's own capture is in flight (a slow owner, as LibreOffice is): B's
    copy is kept and the newer of the two is the clipboard on both;
  - copies on both machines at once end with one clipboard on both; a copy on each side of a
    lost link, the later one wins on reconnect;
  - a delete on A removes B's copy, pinned; a delete delivered before its announcement wins; a
    delete while B is away reaches B when it returns;
  - a tool that copies whatever owns the clipboard (a test client) runs on B: no loop;
  - a password manager's clear on A (owner None) empties B;
  - an entry evicted on B while being fetched.
- Planted bugs for each control, as before.

## Slices

1. `proto`: the clipboard kinds, Latin-1 names, digests, link version 4; `announce.rs` and
   `frame.rs` removed; fuzz.
2. `net`: clipboard streams, windows, limits, tasks, `Event::Clip` and `Handle::clip`; the
   relay's bandwidth limit; relay tests and the latency gate under load.
3. `selection`: deferred replies, `Remote` targets, `Wanted`, `update` in place.
4. `clip` and `store`: `Source::Peer`, `Remote`, the digest, the clock, placing by tick, the
   index's version 2, the forgets.
5. The daemon: announce, receive, fetch, serve, re-select, empty, forget, replay on a new session,
   the loop guard; control version 6, the CLI, the popup, the README; the two-daemon tests.
6. Acceptance on the owner's LAN; the owner uninstalls Synergy (plan §6, M5).

## Red team

### Pass 1, 2026-10-31

A separately briefed reviewer, reading only. On the two departures: claiming at once is right,
given finding 1; fetching everything is right, given bounds (finding 8).

| # | Finding | Resolution |
|---|---|---|
| 1 | A change from a peer, arriving while a slow owner here is still answering, claimed the clipboard and abandoned the user's own copy | Ticks taken at the owner change; never claim over a capture in flight; applied after it, if still newer; a test with a slow owner |
| 2 | `Forget` had no tick and left no trace, on unordered streams: a delete arriving before its announcement lost, and the password stayed | Remembered forgets with ticks refuse an older `Announce`; tested in reverse order |
| 3 | A history merge sent `Forget`, which deletes pinned copies elsewhere; `clear` deleted elsewhere what it spares here | Entries named by digest, so merges send nothing; `clear` follows the local pin rule |
| 4 | A pure Lamport clock let an old copy win after a split, repeated ticks after a crash, and one huge clock pin everyone | Hybrid wall-clock ticks; a tick a day ahead refused |
| 5 | Any other tool that copies the clipboard's owner (CopyQ, spice-vdagent) would loop with us | A capture equal to the current state is no change; 10 changes a second stops announcing for 10 s |
| 6 | The 3 s deadline assumed requestor timeouts that are wrong: Firefox waits 1 s; a fetch could not be moved forward | Timeouts corrected and cited; one fetch at a time, a waiting paste preempts; Firefox's limit documented |
| 7 | Stream priority does not stop a full congestion window or a deep queue from delaying input; a loopback relay would never show it | 256 KiB of clipboard bytes in flight; a bandwidth-limited relay with a queue runs the latency gate |
| 8 | Memory and disk were unbounded (16 fetches of 64 MiB, unbounded announcements, a size field the peer chose); eviction during a fetch unspecified | One fetch at a time, 64 announcements held, no size field (the announcement's, checked with the hash), eviction cancels |
| 9 | An image-only announcement could not be an entry; deduplication ignored pending targets | `Remote` targets count as held; per-target hashes and digests; blobs already held are not fetched |
| 10 | Acknowledgements, re-announce requests, a repeated `Announce`, in-place updates and the target count were undefined | Forgets replayed whole (idempotent); a re-select announces; a repeated `Announce` adds only what is missing; `update` in place |
| 11 | With three machines, nothing is relayed, and states can lag | M5 is two machines, said so |
| 12 | `clear-history` staying local contradicts §5.2; unpairing and the forgets' overflow were undefined; secrets now persist on every machine | Marked **Departs**; unpairing keeps history; the last 1000; the README says it |
| 13 | An older received entry went to the front of history | Placed by tick |
| 14 | No test for the above | Each has a test above |
