# The link

How two Jackstay daemons find, authenticate and talk to each other (milestone M3). Plan §5.5 says
what the link must do; this says how. Input sharing (M4) and clipboard sharing (M5) ride on it.

Status: **design, revised after red-team pass 1** (findings and their resolutions at the end);
slice 1 (`proto::link`, `jackstay-net`) and slice 2 (the daemon's link, the CLI, the two-daemon
harness) built.
Facts marked *measured* come from `spikes/link-probe` (spikes/README.md); facts marked *source*
were read in quinn 0.11.12, quinn-proto 0.11.19 and rustls 0.23.46, 2026-10-29.

## Shape

- A new crate, `jackstay-net`, holds everything that touches the network: identity, the TLS
  verifier, the QUIC endpoints, sessions, pairing, reconnect. It forbids `unsafe`. It has no X.
- Messages between machines are types in `proto` (`link` module), framed as today (`frame.rs`: u32
  length, u8 kind), with the decoder lints, property tests and the fuzz target. Each kind has its
  own size limit, checked before anything is allocated.
- In the daemon, a **link thread** runs a single-threaded tokio runtime with the endpoints and
  every session. The clipboard thread talks to it as it talks to the store and thumbnails threads:
  a channel in, and a channel out with a wake pipe its `poll` watches. In M4 the input thread gets
  its own pair of channels to the link thread, so a keypress never waits behind the clipboard
  thread.
- The link thread never decodes or copies a whole clipboard target. In M5, bulk moves in bounded
  chunks, and if measurement shows chunks still delay input, bulk moves to a second runtime thread.

## Identity

- Each machine has one Ed25519 key pair, made on the daemon's first start with ring's
  `Ed25519KeyPair::generate_pkcs8` and kept as PKCS#8 in `$XDG_STATE_HOME/jackstay/identity`, mode
  0600. It is written to a temporary file, synced, and linked into place with `link(2)`, which
  fails if the name exists, so two daemons starting at once cannot both make one.
- A machine's **public key** is its SubjectPublicKeyInfo, 44 bytes (*measured*). It is the machine's
  only identity; hostnames are addresses, never trusted. Its **fingerprint**, for display, is the
  first 8 bytes of its SHA-256, in hex.
- An existing identity file readable by anyone but its owner is made 0600 when read.
- An identity file that cannot be read or parsed refuses to start the link (the daemon runs on,
  with the link off and the reason in `jackstay status`). It is never silently replaced: that would
  unpair every peer without saying so.
- From the identity, HKDF-SHA256 derives two more keys: the QUIC **stateless-reset key** and the
  **connection-ID key** (`EndpointConfig::new`, `HashedConnectionIdGenerator::from_key`). quinn's
  defaults make both at random on each start (*source*: quinn-proto config/mod.rs:186,
  cid_generator.rs:107). Then a restarted daemon drops the old connection's packets as invalid
  (endpoint.rs:259) and sends no reset, and its peer waits out the idle timeout. Derived keys
  survive a restart, so the restarted daemon resets the old connection at its peer's next packet.

## Peers

`$XDG_STATE_HOME/jackstay/peers`, written only by the daemon (pairing, unpairing): one record per
peer with its name, its public key, and its address (`host` or `host:port`, as the user gave it to
`jackstay pair`). It is written whole (temporary file, sync, rename). A missing file means no
peers. A corrupt one refuses to start the link, as above. There are at most 8 peers; pairing a
peer already paired replaces its record and keeps its session. Our own key is never a peer: a
peers file naming it refuses to start the link (a cloned disk image would otherwise pair with
itself).

## Transport

- One QUIC endpoint, on one UDP socket on `link.port`, default **24810** (Synergy's is 24800, so
  both can run during M4's dogfood). The socket is made with socket2 as `[::]` with `IPV6_V6ONLY`
  off, because quinn's `Endpoint::server` binds a plain std socket and leaves dual-stack to the
  `net.ipv6.bindv6only` sysctl (*source*: quinn endpoint.rs:105). If IPv6 is unavailable, it binds
  `0.0.0.0`.
- TLS 1.3 only, ring as the provider, one signature scheme (Ed25519), and RFC 7250 raw public keys
  on **both** ends. rustls's `AlwaysResolves{Client,Server}RawPublicKeys` present our key. One
  verifier, with `requires_raw_public_keys`, checks the peer's key against the peers file and the
  handshake signature with `verify_tls13_signature_with_raw_key`. TLS 1.2 is refused.
- **No resumption.** rustls by default keeps a 256-entry server session cache, sends 2 tickets per
  session, and on resumption takes the client's key from the ticket without calling the verifier
  (*source*: server/builder.rs:113,123, server/tls13.rs:371; *measured*: with the defaults, three
  reconnections were verified once). An unpaired peer could otherwise resume a session its pairing
  had allowed. So the server has `NoServerSessionStorage` and `send_tls13_tickets = 0`, and the
  client `Resumption::disabled()`. Two more things stand in the way, each enough alone: a change
  of peers rebuilds the server's configuration, and with it an empty session cache; and after
  every handshake the session's key (`peer_identity`) is checked against the peers again, before a
  stream is accepted. A test keeps tickets as an attacker would and is refused after unpairing; no
  single one of the three, removed, makes it fail.
- An unknown key fails the handshake on both ends with alert 48 (*measured*, both directions),
  before any application byte is read. No message from an unpaired machine is ever decoded.
- ALPN `jackstay/1`. A peer speaking another version fails the handshake with
  `no_application_protocol`, which `jackstay status` names as "different Jackstay version". A dial
  that fails in the handshake keeps its reason: a refused key or another version shows as refused,
  and backs off to the refusal ceiling, not as an endless "dialing".
- The server name sent is the constant `jackstay`; it means nothing and is not checked.

### Who dials

Both machines dial each other, and each answers. When both sessions come up, both machines keep
the one dialed by the **smaller public key** (bytewise) and close the other. Both know both keys,
so both choose the same session, and a machine that can reach its peer only one way still gets a
link. While a session is connected, neither dials. Each daemon start picks a random 64-bit **run
id**, sent in `Hello`. A new session whose run id differs from the existing session's replaces it
at once: the peer has restarted, and the old session is dead though not yet reset. A new session
with the same run id is the duplicate above, but only if the kept session is young (kept less
than two dial deadlines ago): a peer dials only when it has no session, so a later one means it
lost its side of ours, and the new session replaces it. After a session is lost, the machine with the larger
key waits 500 ms before dialing, so the preferred session usually comes up alone.

A session counts as connected only once the peer's `Hello` has arrived. A client whose key the
server refuses completes its own side of the handshake first (*measured*: the spike had to wait to
see the refusal), so the dialer writes `Hello` at once, and the answerer replies with its own.

### Liveness

- Status counts, per peer, the sessions kept since the link started (`connects`): a number that
  keeps rising is a link that keeps dropping.
- Each side sends a `Ping { seq, interval }` on stream 0 every `interval` milliseconds, which the
  peer answers with `Pong { seq }`. The last round trip shows in `jackstay status`. Each ping
  states the interval to the next, so the receiver knows when to worry: no message on stream 0
  for **3 of the peer's intervals**, or our oldest ping since we last heard anything unanswered
  for **3 of our own**, makes the session **suspect**. The second rule is what lets an active side
  notice an idle peer's silence in 300 ms. The link thread reports it, and in M4 the
  primary ungrabs and the secondary releases what it injected (below). The interval is 1 s when
  idle, so suspect at 3 s. QUIC's idle timeout is 10 s; after it the session is **dead** and
  closed. Ten seconds tolerates a Wi-Fi power-save pause.
- **Input heartbeat (M3 builds it, M4 uses it).** While a session is *active* (M4: it is the
  secondary the primary is grabbing for), both sides' ping interval drops to 100 ms, and the first
  such ping is sent at once. A side that hears nothing for 300 ms acts. The secondary releases every key and button it injected. The primary
  ungrabs and returns the pointer to its own screen. Silence is detected on both ends, so the
  user's keyboard is never feeding a dead session for longer than 300 ms. A session that closes or
  is replaced does the same at once, without waiting.
- A restarted peer resets the old session at once (Identity, above).
- **Dialing.** Resolving the name is bounded by 1.5 s; if it stalls (DNS, mDNS), the last
  addresses it resolved to are dialed. Each dial attempt has its own deadline of 1.5 s, independent of the 10 s idle
  timeout, which otherwise also bounds a handshake to a black-holed address (*source*:
  connection/mod.rs:323). After a failure the next attempt comes 100 ms later, doubling to at most
  2 s. A refused handshake (a key the peer does not know, another version) doubles to at most 60 s
  instead. The name is resolved again before each attempt, on a blocking-pool thread, and each
  resolved address is tried in turn: an unreachable address listed first costs each attempt its
  1.5 s.
- **Resume from suspend** is seen as a jump between `CLOCK_BOOTTIME` and `CLOCK_MONOTONIC`, checked
  every second. It resets the backoff and redials at once. Network changes are not watched
  (netlink): with 1.5 s attempts and a 2 s ceiling, a link returns within about 4 s of the
  network, plus 1.5 s for each unreachable address the name resolves to first.
- While the port cannot be bound, nothing is dialed, and the link waits for the bind retry
  without waking for the dials it cannot make.

### Streams

- **Stream 0** is the first bidirectional stream, opened by each session's dialer at once. It
  carries only small messages: `Hello`, `Ping`/`Pong`, `Bye`, and in M4 input and pointer motion,
  in order (plan §5.5). It has the highest send priority (`SendStream::set_priority`). quinn
  otherwise shares sending turns equally among streams, and caps each write by the
  connection-wide send window (*source*: quinn-proto connection/streams/state.rs:845, send.rs:30).
- **Clipboard (M5)** traffic never travels on stream 0, announcements included (they carry
  targets up to 64 KiB). Each announcement and each transfer has its own stream, and the
  clipboard's unacknowledged bytes are kept well below the connection's send window, so stream 0
  always has room.
- The first message on stream 0, both ways, is `Hello { version, run, name }`: the protocol
  version (it must equal ours, beyond the ALPN check), the run id, and the machine's hostname. The name is for display
  only, and is shown with control and non-printing characters replaced, as sources are
  (docs/daemon.md, "Control socket").

### Unauthenticated packets

Anyone on the LAN can start a handshake, and each costs a key exchange and a signature on the link
thread that input will use. So: `max_incoming` is 16. An address not yet validated is first sent
a `retry()`, which costs a forged source address a round trip it cannot complete, and spends
nothing. Only validated `Incoming`s are counted against a token bucket (10 per second, burst 10);
beyond it they are `ignore()`d. So forged packets cannot use up the handshakes real peers need. The
harness measures ping round trips while a stranger floods the port.

## Pairing

`jackstay pair HOST` is run on both machines, each naming the other. It lasts at most 2 minutes, or
until it ends. The main endpoint and its verifier are never relaxed. Pairing has its own endpoint.

1. The daemon opens a **pairing endpoint** on `link.port + 1`, with ALPN `jackstay-pair/1`, whose
   verifier accepts any key but our own. It still checks the handshake signature. It has no
   resumption, as above.
2. Each machine dials the other's pairing endpoint, at HOST's addresses, until a session is up.
   Each accepts **one** incoming handshake, and only from an address HOST resolves to. Any other
   handshake attempt ends pairing with nothing recorded: one from another address, or a second one
   while the first is in progress or completed, counted at `Incoming`, before any work. One that
   never completes frees its place: an abandoned dial's closing packet arrives as a handshake, and
   would otherwise make the other machine's real one a "second attempt". This gains an attacker
   nothing, since the code depends on nonces exchanged only after a completed handshake (step 3).
   Of the sessions that come up, the smaller key keeps the first and commits on it; the larger key
   follows: it keeps the session the smaller key's `Commit` arrives on. Any other first message
   on a session before that choice breaks the protocol and ends pairing.
3. **Commit, then reveal.** Each side picks a random 32-byte nonce and sends `Commit
   { SHA-256(nonce) }`. Each sends `Reveal { nonce }` only after it has received the peer's
   `Commit`, and checks that the peer's nonce matches its commitment. The code is the first 4 bytes
   of SHA-256(exporter ‖ nonce of the smaller key ‖ nonce of the larger), mod 10⁶, as six digits.
   The exporter is the TLS exporter (RFC 8446 §7.5, label `EXPORTER-jackstay-pairing`, 32 bytes).
   Why commit: a machine in the middle, acting as the TLS server toward one side, chooses its
   server random and key share after seeing the client's, and could grind them offline until the
   exporter's code matched its other session. With committed nonces, each code depends on a value
   the attacker learns only after it is bound to its own. So one attempt matches with probability
   10⁻⁶, and step 2 allows one attempt.
   A reveal that does not match its commitment, or one before any commitment, ends pairing.
4. Each CLI shows the peer's name, its fingerprint and the code, and asks `yes/no`. A "yes" sends
   `Confirm`. A machine records the peer when it has both its own "yes" and the peer's `Confirm`,
   then sends `Bye`, closes the pairing endpoint, and dials the normal session.
5. Anything else ends pairing with nothing recorded: "no", the 2-minute limit, a closed session,
   the CLI disconnecting, a bad reveal, or a `HOST` that resolves to nothing.

If the session drops between the two `Confirm`s, one machine may have recorded the other and the
other not. Pairing again fixes it, since an existing record for the same key is replaced. One
pairing runs at a time.

`jackstay unpair NAME` (or a fingerprint, when names collide) removes the record and closes its
session.

## In the daemon

- **Control protocol v3** (docs/daemon.md, "Control socket"). v2 capped every request at 16
  bytes, too small for a host. The cap is now 257 bytes, a name of 255 and its length, still
  refused from the header; each field has its own limit inside it. `Link` asks for the link's
  state: the port, our fingerprint, and per peer its name, fingerprint, address, state
  (connected, silent, not connected, refused: unknown key or version), last round trip and
  sessions kept. `jackstay status` shows it too. Pairing is a flow: `Pair { host }` is answered
  `Ok`, and that client then gets `PairCode { name, fingerprint, code }`, then `Paired` or
  `PairFailed { reason, detail }`. It answers with `PairAnswer { yes }`, an ordinary request, so
  v2's rule that a client waiting for a deferred response is not read does not stand in the way.
  There is one pairing client at a time; a second is answered `PairFailed { Busy }`. Pairing is
  cancelled when its client disconnects. `Unpair { peer }` takes a name or a fingerprint, refuses
  a name two peers share, and is answered once the peers file is saved.
- **Status** goes out when anything but a round trip changes, and with the round trip once a
  second: an active session measures ten a second.
- **Configuration** gains `link.port` (1024–65534; pairing uses the next port) and `link.enabled`
  (default true). With no peers the endpoint is still bound, so a peer can be paired.
- A failure to bind the port is reported in status and the journal and retried every 10 s. The
  clipboard is unaffected.
- The journal says when the port opens or fails, each peer's change of state by fingerprint, and
  pairings and unpairings, never message contents (DP-3).
- **Supervision.** A panic in any of the link's tasks is not contained: it ends the link thread
  (a tokio `JoinHandle` does report it, *measured* in slice 1; the engine re-raises it). The
  clipboard loop checks every turn that the thread has not ended and has sent status within 10 s;
  if not, the daemon exits 101 and systemd restarts it, so a wedged link thread restarts the
  daemon too. Silence is counted only while the daemon runs: a gap of 3 s between the loop's
  checks means the whole process was paused (suspend, a frozen VM, swapping), and the link's
  silence then is forgiven. On the exit, a thread that stopped answering is left, not joined; the
  runtime gives blocking work (a stalled name lookup) half a second. The failures are counted in
  `$XDG_STATE_HOME/jackstay/link-failures`; after three in a row the next start runs with the
  link off and says why, then forgets the count, and a link that runs a minute forgets it too. So
  a fault that recurs costs the link, never the clipboard. The runtime is built before the thread
  starts, so failing to build it turns the link off rather than ending the thread.
- **Saving the peers file** happens on tokio's blocking pool, one change at a time, worked out
  from the peers as they are when its save starts; a change takes effect only once it is on disk.
  A slow disk never holds up pings.

## Dependencies this adds

`quinn` (features `runtime-tokio`, `rustls-ring` only), `rustls` (`ring`, `std`; no TLS 1.2, no
aws-lc), `ring` (also HKDF and random nonces), `tokio` (`rt`, `net`, `time`, `sync`), `socket2`
(already in quinn's graph). Together, 27 crates not already in the graph (*measured*), the largest
addition so far, which plan §4.3 accepted for QUIC. New licenses: ISC (ring, untrusted,
rustls-webpki) and BSD-3-Clause (subtle), both GPL-compatible. Each becomes a named exception in
`deny.toml`, not a global allowance. ring builds C and assembly with the container's compiler.

## Testing

- **In-process, `jackstay-net`** (built; `crates/net/tests/link.rs`, `files.rs`). Each control
  below was proven by planting its removal and watching its test fail: derived reset keys, the
  duplicate rule (with neither machine deferring, four rounds), the second-handshake count (a
  second handshake with the *same* key, from a copy of a machine's disk, since a different key is
  caught by the same-machine check first), the three-interval suspicion, and the refusal kept from
  a failed dial. One plant survives, accepted: without the run-id rule, a restarted peer is still
  linked again within 2 s, because on loopback the old session is reset by the next packet before
  the restarted peer's deferred dial arrives; the rule saves at most a second where no packet
  draws a reset first. Nor does turning server resumption back on make the ticket test fail:
  the configuration rebuilt on unpairing and the check after the handshake each refuse alone.
  After the code review (pass 2 below), 13 more plants, each caught: the bind-wait spin, the
  active side's own-ping suspicion, the peer-interval suspicion (against an active peer), a
  reveal not checked against its commitment, an early reveal ignored, the pairing address check,
  re-pairing replacing a slot, our own key as a peer, and status sent on every round trip.
  The planned cases:
  - two endpoints on loopback, pinned both ways;
  - an unknown key either way, refused;
  - a resumption attempt after unpairing, refused;
  - the main endpoint refusing a stranger while pairing runs;
  - the pairing endpoint refusing a second attempt, and one from the wrong address;
  - a bad reveal;
  - the duplicate rule (both dial; both keep the same session);
  - a one-way-reachable pair still linking;
  - replacement by a new session from the same key;
  - **the answerer restarted on the same port: the dialer notices within 1 s** (reset);
  - a black-holed dial giving up at 1.5 s;
  - suspect at 3 s, and at 300 ms when active.
- **Two daemons, one container** (built; `crates/jackstayd/tests/link.rs`; the CLI's side of
  pairing against a scripted daemon in `crates/jackstay/tests/cli.rs`): each with its own Xvfb,
  home and ports, on 127.0.0.1, driven through their control sockets. The relay owns the address
  each machine knows the other by, so every datagram passes it. The forged flood replays one real
  handshake datagram from a fresh port each time, 500 a second, never answering the Retry; the
  machine flooded cannot dial out (a wrong address in its peers file), so only its peer's dials can
  link them. The machine in the middle answers one side's pairing dial and dials the other,
  copying the stream unchanged, with its key between the two machines', so both order the nonces
  alike and only the TLS exporter can tell the codes apart: the test asserts the two codes
  differ (a user who compares them, as the CLI asks, then says no). A rebinding is survived only
  at the machine that dialed the kept session, QUIC's client; at the answering machine (QUIC's
  server, which may not move) quinn drops the client's packets from the new address, and the link
  is recovered by dialing again. Each control below was proven by planting its removal: Retry
  before the handshake limit, pairing cancelled when its client leaves, a second pairing client
  refused, the exporter in the code, the failure count, and rebinding the dialing side (the
  answering side, planted, fails). Two plants survive, accepted. Without the stale-duplicate rule
  (pass 2, 5), one-way loss still recovers soon after it ends, because the stale side's next ping
  draws a reset, as with the run-id rule. Without the pause forgiveness, the 30 s stop still
  passes: on resume the link thread's overdue status usually reaches the clipboard loop in the
  same turn, before its check; the forgiveness is for the turn where it does not. Not built: a
  pairing that times out (two minutes; the in-process tests cover it), and a cancelled pairing's
  last event arriving after a new one starts (a race; the session refuses the new one until it
  has). Planned: Pairing with both `yes`, one `no`, a timeout, the CLI
  killed mid-pairing, and a mismatched pair of hosts. Plan §5.7 said `tc netem` between
  containers. Instead, a UDP relay in the test process sits between the two daemons. It needs no
  privileges and no second container, and its faults are exact. Cases:
  - black-hole for 2 s: no reconnect;
  - black-hole for 12 s: dead, then linked again within 4 s of resuming;
  - one-way loss;
  - 5 % loss with 2 ms delay: no reconnect, and a bound on the ping round trips;
  - reordering and duplication;
  - a change of the relay's source port (rebinding);
  - packets above 1280 bytes dropped;
  - either daemon killed and restarted;
  - one daemon stopped (SIGSTOP) for 30 s and continued, standing in for suspend;
  - **an active man in the middle:** the relay terminates pairing toward both sides with its own
    key, and pairing must fail;
  - a handshake flood from a stranger, with pings still within budget.
- **Fuzzing:** the `link` messages join the `proto` fuzz target and corpus.
- **Latency budget** (plan §5.7) starts in M4, where there is input to time. M3 records the ping
  round trips in the harness to set it, and measures whether quinn's ACK-frequency extension helps
  stream 0 under loss.

## Slices

1. `proto` link messages, and `jackstay-net`: identity, verifier, endpoints, sessions, liveness,
   pairing, with the in-process tests.
2. The daemon's link thread, peers file, configuration, status, control protocol v3, and
   `jackstay pair` / `unpair`. The two-daemon harness with the relay.

Plan §6 also lists, under M3, `proto` adopting `clip`'s lossless `TargetName`. Nothing sends an
`Announce` until clipboard sharing, so it moves to M5, where its fuzz corpus is grown against real
use.

## Red team

### Pass 1, 2026-10-29

A separately briefed reviewer, which read the vendored sources. Findings 2, 3 and 9 were checked in
those sources before being accepted.

| # | Finding | Resolution |
|---|---|---|
| 1 | Pairing code ground by half-handshakes against one endpoint | Accepted, and found worse: an attacker acting as TLS server grinds offline. Commit-then-reveal nonces in the code; one handshake per pairing, from HOST's addresses, counted at `Incoming` |
| 2 | Resumption skips the verifier (server/tls13.rs:371) | Accepted. No session storage, no tickets, client resumption off; the key is checked against peers after every handshake |
| 3 | Restarted peer sends no stateless reset: random reset and CID keys | Accepted. Both derived from the identity by HKDF; harness restarts the answerer |
| 4 | Clipboard bulk can starve stream 0 (shared send window, equal priority) | Accepted. Stream 0 highest priority; clipboard never on stream 0; clipboard's unacknowledged bytes bounded; per-kind frame limits; no bulk copying on the link thread |
| 5 | A dial to an unreachable host lasts 10 s | Accepted. 1.5 s per attempt; redial at once on resume (clock jump) |
| 6 | The primary cannot tell a dead secondary quickly | Accepted. 100 ms pings while active, silence acted on at 300 ms on both ends; close releases at once. Built in M3, as plan §6 lists |
| 7 | Unauthenticated handshakes cost CPU on the input thread | Accepted. `max_incoming` 16, token bucket, `retry()`; flood case |
| 8 | Control socket: 16-byte request cap, no mid-flow requests | Accepted. Control v3: per-kind limits, pairing as a flow with `Confirm` |
| 9 | `[::]` dual-stack depends on a sysctl | Accepted. socket2, `IPV6_V6ONLY` off, fallback to `0.0.0.0`; every resolved address tried |
| 10 | Smaller-key-dials makes reachability one-way | Accepted. Both dial; the smaller key's session is kept |
| 11 | Pairing relaxes the shared verifier for all dials | Accepted. Separate pairing endpoint and port; the main verifier never relaxed |
| 12 | A dial "connects" before the server's refusal arrives | Accepted. Connected only on the peer's `Hello`; the dialer writes first |
| 13 | Harness misses loss, reordering, rebinding, MTU, suspend, MITM, flood | Accepted. All added; stronger pass conditions |
| 14 | Refused peers redialed every 2 s; peer-chosen names; spike label mismatch; own key; watchdog blind to the link thread | Accepted. 60 s backoff for refusals; names sanitized, fingerprints shown, unpair by fingerprint; label fixed in this doc (the spike's label was a placeholder); own key refused; watchdog fed only while the link thread reports in. ACK frequency to be measured |

### Pass 2 (code review of slice 1), 2026-10-29

A separately briefed reviewer, read-only, on the code against this design.

| # | Finding | Resolution |
|---|---|---|
| 1 | The link thread spins while the port cannot be bound (dials due but impossible) | Fixed: no dial wake-ups while unbound; a skipped dial moves on. Test measures the child's CPU time |
| 2 | An active side waits 3 of an idle peer's intervals (3 s) to suspect it | Fixed: also suspect after 3 of our own unanswered intervals. Mirror test |
| 3 | The token bucket was charged before `retry()`, so forged sources starve real peers | Fixed: Retry first, only validated handshakes counted. Flood case in the two-daemon harness |
| 4 | Pairing's commitment check, early reveal and address check untested | Fixed: a test peer that breaks the protocol (wrong reveal, early reveal, both key orders; another source address) |
| 5 | A late same-run session from the "wrong" dialer was closed and a dead one kept | Fixed: the duplicate rule applies only while the kept session is young. One-way loss case in the harness |
| 6 | Name resolution unbounded; unreachable addresses lengthen recovery | Fixed: 1.5 s bound with the last good answer kept; the recovery bound above corrected |
| 7 | Child processes leak when a test fails | Fixed: a guard kills on drop; children exit when their standard input closes |
| 8 | Dials while a session awaits `Hello`; re-pairing replaced the slot | Fixed: no dial with a session pending; re-pairing keeps the slot. Test |
| 9 | An abandoned dial's closing packet could take pairing's one incoming place | Fixed: a handshake that never completes frees the place (step 2) |
| 10 | Round trips longer than the ping interval never measured | Fixed: a ring of the last 16 pings |
| 11 | Own key not refused on load; identity mode; doc's pairing choice; `TooManyPeers` on re-pair | Fixed: refused at start; mode made 0600; doc corrected; limit checked when the peer is recorded |
| 12 | Status sent on every round trip (ten a second when active) | Fixed: round trips go out on the 1 s tick. Test counts events |
| 13 | Ports by bind-then-release; tight bounds | Fixed: a per-process port range, never reused; bounds widened |

### Pass 3 (code review of slice 2), 2026-10-29

A separately briefed reviewer, read-only.

| # | Finding | Resolution |
|---|---|---|
| 1 | A whole process paused 10 s or more is taken for a wedged link thread (101) | Fixed: a 3 s gap between checks forgives the silence; the suspend test stops for 30 s and checks the daemon still runs |
| 2 | The rebind test depended on key order: only QUIC's client may move | Fixed: the dialing side rebinds; the answering side's limit documented above. That fix still took the smaller key for the dialer, which holds only usually: a session the larger key dialed can come up alone (CI, 2026-10-30; 1 run in 30 here). The test now asks the daemon who dialed (control version 4, `Peers`); planted inverted, it fails every run |
| 3 | On a link failure, the exit joined a wedged thread; a stalled lookup held up shutdown | Fixed: the handle is abandoned; the runtime shuts down within 0.5 s |
| 4 | Saving the peers file blocked the engine | Fixed: saved on the blocking pool, one change at a time |
| 5 | A cancelled pairing's last event could be taken for a new one's | Fixed: the session refuses a new pairing until the cancelled one has ended |
| 6 | A link fault that recurs restarts the daemon until systemd gives up | Fixed: the runtime built at start (failure: off); after three failures in a row, one start with the link off |
| 7 | A version 2 client was refused after an upgrade | Fixed: versions 2 and 3 greeted, each in its own |
| 8 | Unpairing what is not paired was reported as a failed save | Fixed: its own error, shown as no such peer |
| 9 | Fingerprints and codes printed uncleaned; a host that does not parse called unresolvable | Fixed: all printed text cleaned; its own reason |
| 10 | Docs ahead of the tests and code | Fixed: the 30 s stop, both kinds of restart, the flood measured while linked, the journal lines, and this list |
| 11 | Port reuse under one-process-per-test runners; a marginal flood rate | Accepted for cargo test (one process per binary); the rate bound halved |
