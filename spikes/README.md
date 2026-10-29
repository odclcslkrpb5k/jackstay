# Spikes

Throwaway experiments that answer a design question in `docs/plan.md`. Not part of the product
and not held to the gate; each records its question, how it was run, and its answer.

- `x11-probes`: M0 spikes. Do XFixes pointer barriers act on XTest motion under Xvfb (§5.7)? Do
  keys pressed through XTest stay down after the injecting client dies (§5.3)?
  Run with `./spikes/x11-probes/run.sh` (inside the dev container, via `./dev`).
- `link-probe`: M3 spike (2026-10-29). Can quinn 0.11.12 with rustls 0.23.46 (ring) authenticate
  both ends by raw Ed25519 public keys (RFC 7250), refuse an unknown key in the handshake, and give
  both ends one exporter value for a pairing code? What does a small message's round trip cost?
  Run with `./dev bash -c 'cd spikes/link-probe && cargo run --release'`. Answers, in the dev
  container on the owner's workstation: yes to all three. An unknown key, on either end, fails the
  handshake with alert 48 on both ends. The exporter is equal on both ends and differs per session.
  A 16-byte message's round trip on one stream over loopback: p50 26 µs, p99 49 µs, max 93 µs
  (19,000 samples).
  Resumption, measured 2026-10-29 with a variant of the same probe: with rustls's default server
  session cache and tickets, a client reconnecting three times was checked by the server's
  verifier once. Resumed sessions skip the verifier, as docs/link.md says; the link turns
  resumption off.
