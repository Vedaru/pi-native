# ADR 0005: Drop the TUI — a unit is headless

- Status: Accepted
- Date: 2026-10-05
- Supersedes: ADR 0002
- Deciders: pipelets maintainers

## Context

ADR 0002 chose a native Rust TUI (cell buffer + differential ANSI writer, with a
JS component bridge) and VED-307 built it as `pi-tui`, wired to a
`pipelets --client` mode. In practice the client was poor and, more importantly,
it contradicted the product: **a pipelets unit is a headless worker**. It prints,
serves RPC, or drives the gateway; it never draws a terminal.

Two things made the case decisive:

- The `scripts/headless_gate.py` gate already states the rule: a headless swam
  worker must not link a TUI, clipboard, or image codec. `--client` linked one.
- The TUI added a crate (`pi-tui`, 1,660 lines + `base64` + `unicode-width`), a
  CLI mode, and a maintenance surface that no swarm node uses. Terminal
  interaction is not this project's job.

## Decision

Remove the TUI and the `--client` mode.

- Delete `crates/pi-tui` and its `base64`/`unicode-width` dependencies.
- Delete `pipelets --client` and its renderer/editor/dialog code.
- Add `pi-tui` to the `headless_gate.py` forbidden set, so a UI crate cannot
  re-enter `pipelets`'s dependency tree unnoticed.
- Update the docs: `--client` is gone, and scope.md records the TUI as dropped.

## Consequences

- `pipelets` is a strict headless core: `--print`/one-shot, `--serve` (RPC),
  `--gateway`. Interactive clients (a web UI, a robot, a script) attach to those
  surfaces instead of a bundled terminal UI.
- One fewer crate and two fewer dependencies; the release binary shrinks.
- A user who wants a terminal client can build one against the RPC/gateway
  protocol, exactly as pi-web does. If a first-party terminal client is ever
  wanted, it should be its own crate that depends on the core, never a mode
  inside the worker.

## Alternatives considered

- **Keep `pi-tui`, drop only `--client`.** Leaves a dead crate in the tree and
  contradicts the gate's intent.
- **Feature-gate the TUI.** Works, but a swarm worker should not carry the
  machinery at all; the gate is the cleaner guard.
