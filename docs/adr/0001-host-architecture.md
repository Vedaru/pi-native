# ADR 0001: Host architecture and extension strategy

- Status: Accepted
- Date: 2026-10-04
- Issue: VED-303
- Deciders: pi-native maintainers

## Context

pi is a TypeScript/Node application. Its idle RSS is ~111 MB, of which ~58 MB is
the Node/V8 runtime itself. The project goal is to reduce memory by rewriting
only the memory-heavy subsystems in Rust and removing the Node runtime.

Two hard constraints shape the host choice:

1. **Provider-side identity (VED-315).** Outbound provider requests and
   prompt-cache behavior must be indistinguishable from pi.
2. **Extension compatibility.** pi loads arbitrary JS/TS extensions at runtime
   via `jiti`. If we drop Node, something must still run that code.

A reference native port (`Dicklesworthstone/pi_agent_rust`) was measured at
**38.9 MB** idle RSS and demonstrates a working architecture.

## Decision

1. **Native Rust host.** The shipped runtime is a Rust process. No Node/V8.
2. **Embedded QuickJS for extensions** via `rquickjs`, with `swc` performing
   TypeScript-to-JavaScript transpilation (replacing `jiti`). JS/TS extension
   entrypoints load unchanged.
3. **Hostcall bridge.** Extensions call the host through a Promise-based
   `pi.tool/pi.exec/pi.http/pi.ui/pi.events` bridge; the host enforces a
   capability policy per request.
4. **Native providers and HTTP.** SSE streaming implemented natively (no
   `undici`, no provider SDKs).
5. **Native TUI** via `crossterm` (plus a component layer we control).
6. **Optional `wasmtime`** behind a feature flag, only if a WebAssembly
   polyfill is needed for extensions that require it.

The provider request path is ported from pi and gated by byte-level parity
before it is considered done.

## Rationale

- Removes the ~58 MB Node/V8 floor; the reference shows ~39 MB is achievable
  today, and our target is <= 25 MB.
- `rquickjs` + `swc` is the lowest-risk way to keep pi's extension ecosystem,
  which is a defining feature; a new extension ABI would break it.
- Keeping extensions in-process avoids IPC/serialization overhead and keeps the
  memory win.

## Alternatives considered

- **Bun/JSC compiled binary (interim).** Keeps JS extensions and drops Node
  install, but keeps a JS engine and GC, so the memory floor stays high. Useful
  as a fallback, not the target.
- **Pure native extension ABI (WASM/native descriptors).** Lowest memory, but
  breaks the JS extension ecosystem and requires rewriting every extension.
- **Out-of-process JS sidecar.** Preserves the Rust host but adds another
  process baseline and IPC marshalling through the live object graph; rejected
  for the hot path.

## Consequences

- We ship a JS engine (QuickJS) inside the binary. QuickJS is small relative to
  V8 and is the accepted cost of extension compatibility.
- Transpilation and module loading must match enough of `jiti`'s behavior for
  real extensions to load.
- Runtime selection is by entrypoint type (JS/TS vs native descriptor).
- Memory budget: idle headless <= 25 MB; steady-state TUI <= 60 MB.

## Reversibility

The extension engine is behind a trait boundary. If QuickJS proves too costly,
we can move to an out-of-process worker without changing the host protocol.
