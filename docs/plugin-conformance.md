# Plugin conformance

How many of pi's own example extensions load through the pipelets QuickJS host.

Reproduce:

```bash
cargo run -p pi-plugins --example conformance -- /path/to/pi/packages/coding-agent/examples/extensions
```

## Result (2026-10-04)

**84 / 87 (96.6%) load and invoke their factory.**

The wrapper is deliberately **generic** (see ADR 0001). There are no
per-package or per-export branches: one import rewrite, one name-agnostic proxy
stub, one `pi` API proxy, one Node built-in mapping table.

## How the generic adapter works

1. **Import rewrite** (`pi-transpile`): named/namespace imports from bare
   packages become `import __ns from "pkg"; const X = __ns.X;`. This sidesteps
   ESM's static export checking so a proxy can satisfy any name.
2. **Proxy stub** (`pi-plugins`): every bare package resolves to one proxy whose
   properties are callable and constructable. Plugins load and register even
   when the library's behavior is absent.
3. **`pi` API proxy** (`globals` prelude): unknown extension methods record a
   hostcall instead of throwing.
4. **Node built-in table**: `fs`, `path`, `os`, `crypto`, `events`,
   `child_process`, `buffer`, `zlib`, `util`, `readline`, `url`, `module`, ...,
   each backed by Rust (some capability-gated).

## Remaining failures (3)

These are genuine behavior gaps, not missing adapter surface:

- `factory: not a function` — a stubbed library returns something the plugin
  calls.
- `factory: cannot read property 'startsWith' of undefined` — a stubbed value
  where a string was expected.
- `factory: readSchema is not initialized` — a stubbed library needs real
  initialization.

Per the adapter principle, these are **not** fixed by adding name-specific
branches. If they matter, the fix is a generically-provided module in the native
core, not a stub special case.

## Not yet wired

- Tool/command **handlers** are registered but not invoked by a runtime; that
  requires the session/tool core.
- `createRequire` is unsupported (npm packages that dynamically require native
  modules cannot run).
