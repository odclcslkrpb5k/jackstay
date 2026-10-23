# Spikes

Throwaway experiments that answer a design question in `docs/plan.md`. Not part of the product
and not held to the gate; each records its question, how it was run, and its answer.

- `x11-probes`: M0 spikes. Do XFixes pointer barriers act on XTest motion under Xvfb (§5.7)? Do
  keys pressed through XTest stay down after the injecting client dies (§5.3)?
  Run with `./spikes/x11-probes/run.sh` (inside the dev container, via `./dev`).
