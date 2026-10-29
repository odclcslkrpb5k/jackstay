# The link

How two Jackstay daemons find, authenticate and talk to each other (milestone M3). Plan §5.5 says
what the link must do; this says how. Input sharing (M4) and clipboard sharing (M5) ride on it.

Status: **design, revised after red-team pass 1** (findings and their resolutions at the end).
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
peers. A corrupt one refuses to start the link, as above. There are at most 8 peers. Our own key
is never a peer (a cloned disk image would otherwise pair with itself).

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
  (*source*: server/builder.rs:113,123, server/tls13.rs:371). An unpaired peer could otherwise
  resume a session its pairing had allowed. So the server has `NoServerSessionStorage` and
  `send_tls13_tickets = 0`, and the client `Resumption::disabled()`. Independently, after every
  handshake, the session's key (`peer_identity`) is checked against the peers file again before a
  stream is accepted.
- An unknown key fails the handshake on both ends with alert 48 (*measured*, both directions),
  before any application byte is read. No message from an unpaired machine is ever decoded.
- ALPN `jackstay/1`. A peer speaking another version fails the handshake with
  `no_application_protocol`, which `jackstay status` names as "different Jackstay version".
- The server name sent is the constant `jackstay`; it means nothing and is not checked.

### Who dials

Both machines dial each other, and each answers. When both sessions come up, both machines keep
the one dialed by the **smaller public key** (bytewise) and close the other. Both know both keys,
so both choose the same session, and a machine that can reach its peer only one way still gets a
link. A session from a peer that already has one, other than that duplicate, replaces it: the peer
has restarted, or lost the old one first.

A session counts as connected only once the peer's `Hello` has arrived. A client whose key the
server refuses completes its own side of the handshake first (*measured*: the spike had to wait to
see the refusal), so the dialer writes `Hello` at once, and the answerer replies with its own.

### Liveness

- Every 1 s, stream 0 carries a `Ping`, which the peer answers with a `Pong`. The last round trip
  shows in `jackstay status`. A session with no packet for **3 s** is **suspect**: the link thread
  reports it, and in M4 the primary ungrabs and the secondary releases what it injected (below).
  QUIC's idle timeout is 10 s. After it, the session is **dead** and closed. Ten seconds tolerates
  a Wi-Fi power-save pause.
- **Input heartbeat (M3 builds it, M4 uses it).** While a session is *active* (M4: it is the
  secondary the primary is grabbing for), the ping interval drops to 100 ms. A side that hears
  nothing for 300 ms acts. The secondary releases every key and button it injected. The primary
  ungrabs and returns the pointer to its own screen. Silence is detected on both ends, so the
  user's keyboard is never feeding a dead session for longer than 300 ms. A session that closes or
  is replaced does the same at once, without waiting.
- A restarted peer resets the old session at once (Identity, above).
- **Dialing.** Each dial attempt has its own deadline of 1.5 s, independent of the 10 s idle
  timeout, which otherwise also bounds a handshake to a black-holed address (*source*:
  connection/mod.rs:323). After a failure the next attempt comes 100 ms later, doubling to at most
  2 s. A refused handshake (a key the peer does not know, another version) doubles to at most 60 s
  instead. The name is resolved again before each attempt, on a blocking-pool thread
  (getaddrinfo can stall), and each resolved address is tried in turn.
- **Resume from suspend** is seen as a jump between `CLOCK_BOOTTIME` and `CLOCK_MONOTONIC`, checked
  every second. It resets the backoff and redials at once. Network changes are not watched
  (netlink): with 1.5 s attempts and a 2 s ceiling, a link returns within about 4 s of the
  network.

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
- The first message on stream 0, both ways, is `Hello { version, name }`: the protocol version
  (it must equal ours, beyond the ALPN check) and the machine's hostname. The name is for display
  only, and is shown with control and non-printing characters replaced, as sources are
  (docs/daemon.md, "Control socket").

### Unauthenticated packets

Anyone on the LAN can start a handshake, and each costs a key exchange and a signature on the link
thread that input will use. So: `max_incoming` is 16. Accepted `Incoming`s are limited by a token
bucket (10 per second, burst 10), and beyond it they are `ignore()`d. An address not yet validated
is sent a `retry()`, which costs a forged source address a round trip it cannot complete. The
harness measures ping round trips while a stranger floods the port.

## Pairing

`jackstay pair HOST` is run on both machines, each naming the other. It lasts at most 2 minutes, or
until it ends. The main endpoint and its verifier are never relaxed. Pairing has its own endpoint.

1. The daemon opens a **pairing endpoint** on `link.port + 1`, with ALPN `jackstay-pair/1`, whose
   verifier accepts any key but our own. It still checks the handshake signature. It has no
   resumption, as above.
2. Each machine dials the other's pairing endpoint once, at HOST's addresses. Each accepts **one**
   incoming handshake, and only from an address HOST resolves to. Any other handshake attempt
   ends pairing with nothing recorded. That includes a second one, or one from another address,
   whether or not it completes, counted at `Incoming`, before any work. When both sessions are up,
   the one dialed by the smaller key is kept, as above.
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

- **Control protocol v3.** Each request kind has its own size limit (v2 capped every request at 16
  bytes, too small for `Pair { host }`). Pairing is a flow, like `Subscribe`. `Pair { host }` is
  answered as it proceeds (`PairCode { name, fingerprint, code }`, `Paired`, `PairFailed { why }`).
  The client may send `Confirm { yes }` while the flow is open, which v2's "not read while waiting
  for a deferred response" forbids for other requests. The flow ends if the client disconnects.
  There is one pairing client at a time. `Unpair { peer }` is an ordinary request. `Status` gains
  the link: the port, our fingerprint, and per peer its name, fingerprint, state (connected,
  dialing, suspect, refused: unknown key or version) and last round trip.
- **Configuration** gains `link.port` (1024–65534; pairing uses the next port) and `link.enabled`
  (default true). With no peers the endpoint is still bound, so a peer can be paired.
- A failure to bind the port is reported in status and the journal and retried every 10 s. The
  clipboard is unaffected.
- The journal names peers and states, never message contents (DP-3).
- **Supervision.** A panic in the link thread ends the daemon with 101, as for the other threads.
  Whether a tokio task's panic is contained by its `JoinHandle` is measured in slice 1 (plan §5.8);
  we do not rely on it. The link thread reports in to the clipboard loop every second, and the
  systemd watchdog is fed only while it does, so a wedged link thread restarts the daemon too.

## Dependencies this adds

`quinn` (features `runtime-tokio`, `rustls-ring` only), `rustls` (`ring`, `std`; no TLS 1.2, no
aws-lc), `ring` (also HKDF and random nonces), `tokio` (`rt`, `net`, `time`, `sync`), `socket2`
(already in quinn's graph). Together, 27 crates not already in the graph (*measured*), the largest
addition so far, which plan §4.3 accepted for QUIC. New licenses: ISC (ring, untrusted,
rustls-webpki) and BSD-3-Clause (subtle), both GPL-compatible. Each becomes a named exception in
`deny.toml`, not a global allowance. ring builds C and assembly with the container's compiler.

## Testing

- **In-process, `jackstay-net`:**
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
- **Two daemons, one container:** each with its own Xvfb, home and ports, on 127.0.0.1, driven
  through their control sockets and the CLI. Pairing with both `yes`, one `no`, a timeout, the CLI
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
