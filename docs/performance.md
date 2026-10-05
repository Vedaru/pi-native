# Performance: memory and CPU

pipelets is built to be a **low-memory, low-CPU bare core** so a swarm of units
is cheap to run. Every figure below was measured on the host with the release
build on 2026-10-05 (`x86_64-unknown-linux-gnu`, `lto="fat"`, `opt-level=2`,
`panic="abort"`, stripped).

Reproduce:

```bash
cargo build --release
export PIPELETS_BIN="$PWD/target/release/pipelets"

python3 scripts/mem_bench.py --target pipelets          # idle RSS vs pi-node
python3 scripts/stress_gate.py --tools ls grep find edit read
python3 scripts/stress_gate.py --session --tools read --turns 20000
python3 scripts/swarm_stress.py --units 32 --mode idle
```

## Binary and idle

| | pipelets | pi-node |
| --- | --- | --- |
| shipped runtime | one **12.4 MB** binary (4.6 MB tarball) | Node + `node_modules` |
| cold-idle RSS (`--rpc`) | **4.4 MB** | 111.3 MB |
| cold-idle RSS (`--gateway`) | **5.0 MB** | — |
| ratio | — | **0.04×** (≈25× smaller) |
| idle CPU (3 s) | **0.001 s** (no busy-wait) | — |

A bare Rust `fn main` reports ~2.3 MB, mostly shared libc, so ~4.4 MB is close
to the floor for a process that has loaded the agent loop, tools, RPC, and the
plugin host. A static build idles lower (~3.3 MB) but stops sharing libc pages
across a swarm, so the dynamic build is kept.

The committed baseline is `artifacts/mem_bench.json`; `scripts/mem_gate.py`
fails CI when an absolute figure regresses or when a native target exceeds 60%
of pi-node's RSS.

## Tools under load (`--stress`)

`pipelets --stress N --stress-tool <ls|grep|find|edit|read>` calls the tool
directly (no agent loop, no transcript), so the numbers are the tool's own
memory and CPU. 20,000 calls each:

| Tool | Wall | Peak RSS | CPU |
| --- | --- | --- | --- |
| `ls` | 0.03 s | 5.2 MB | 0.03 s |
| `grep` | 0.32 s | 6.5 MB | 0.32 s |
| `find` | 0.32 s | 6.3 MB | 0.32 s |
| `edit` | 0.08 s | 5.4 MB | 0.08 s |
| `read` (40 KB each, 819 MB total) | 0.38 s | 5.2 MB | 0.38 s |

Tools stream with bounded buffers: a 200 MB file read peaks at ~5 MB. Without
the window, 50,000 `ls` turns: **0.07 s / 5.1 MB**.

## Long sessions (compaction)

`--stress-session` runs the agent loop over one growing transcript with
token-based compaction (pi's rule: compact when estimated tokens exceed
`contextWindow - reserveTokens`, reserve 16,384). 20,000 turns:

| Workload | Compactions | Peak RSS | CPU |
| --- | --- | --- | --- |
| `ls` (tiny messages) | 5 | 8.8 MB | 0.04 s |
| `read` (40 KB results) | 1,176 (~1 per 17 turns) | 6.1 MB | 0.40 s |

The kept tail is capped at half the threshold, so compaction cannot churn (a
misconfigured window/reserve once caused ~2 compactions per turn). Growth is
proportional to the retained transcript (~900 B/turn), with no leak.

Two fixes came out of pressure testing:

- The loop **cloned the whole transcript every iteration** (O(n²)). It now
  borrows (`CompletionRequest` holds slices): 10k turns went from 9.7 s to
  0.09 s, and 50k from a timeout to under 3 s.
- The loop **accumulated every event**, duplicating tool output for the whole
  turn. `run_with` streams events to a sink; `run` collects them for callers
  that want a `Vec`.

## Swarm cost

`scripts/swarm_stress.py` runs N units at once and reports aggregate and
per-unit RSS (release build):

| Swarm | Per unit | Total |
| --- | --- | --- |
| 8 idle (`--rpc`) | 4.4 MB | 35.2 MB |
| 32 idle (`--rpc`) | 4.4 MB | 141.7 MB |
| 8 busy (`read` sessions, 5k turns) | 6.2 MB | 49.3 MB |

Because the dynamic build shares libc pages, a swarm costs less than the
per-unit sum suggests. An idle unit is ~4.4 MB, so a 32-agent swarm fits in
~143 MB — the same order as **one** pi-node process.

## Images (`read` attachments)

An agent reads what an image *says*, not its full fidelity, so every image is
downscaled to a small, consistent target: **1024 px on the long edge, 1 MiB
encoded** (`PIPELETS_IMAGE_MAX_DIM` / `PIPELETS_IMAGE_MAX_BYTES`). Release
build, through the `read` tool (peak = `VmHWM`, idle baseline ~3 MB):

| input | output | payload | pipelets peak | pipelets time | pi peak / time |
| --- | --- | --- | --- | --- | --- |
| 1920x1080 screenshot PNG (1.5 MB) | 1024x576 JPEG | 90 KB | **17 MB** | 31 ms | 124 MB / 76 ms |
| 2560x1440 screenshot PNG (2.6 MB) | 1024x576 JPEG | 81 KB | **17 MB** | 30 ms | 205 MB / 750 ms |
| 3840x2160 screenshot PNG (5.9 MB) | 1024x576 PNG | 1.0 MB | **17 MB** | 40 ms | 284 MB / 952 ms |
| 3024x4032 phone photo JPEG (3.8 MB) | 768x1024 JPEG | 273 KB | **19 MB** | 51 ms | 361 MB / 1371 ms |
| 6000x4000 photo JPEG (7.4 MB) | 1024x683 JPEG | 261 KB | **19 MB** | 77 ms | 523 MB / 1735 ms |

Peak is ~17-19 MB regardless of input and the payload is <=1 MiB. pi keeps the
full 2000x2000 (1-2 MB payloads) and peaks at 124-523 MB. This is a deliberate
divergence from pi on the request bytes: the target is a policy choice, not a
parity requirement (parity covers request structure), because the agent needs
the content. `PIPELETS_IMAGE_MAX_DIM=2000` restores pi's exact target.

A 7000px source bottoms out at the same flat peak (peak, then RSS 3 s later):

| source | output | payload | peak | after |
| --- | --- | --- | --- | --- |
| 2100x2100 PNG | 1024x1024 PNG | 23 KB | 14 MB | 5.1 MB |
| 7000x7000 PNG | 1024x1024 PNG | 1.0 MB | 22 MB | 5.8 MB |
| 7000x7000 JPEG | 1024x1024 JPEG | 1.0 MB | 40 MB | 6.3 MB |

How it decodes (header first, so the dimensions are known before any pixels):

- **JPEG** uses `libjpeg-turbo-rs`'s scaled IDCT (all 16 libjpeg-turbo factors):
pick the smallest whose output still covers the target, so a large JPEG is
never decoded at full size. It also converts CMYK/YCCK to RGB itself.
- **PNG** is decoded scanline by scanline and box-downsampled on the fly (a
per-row accumulator, not a whole-image one), so a large PNG never allocates
more than the reduced bitmap.
- **Orientation** is applied *before* the target math, so an EXIF-rotated photo
resizes to the oriented dimensions, and the scaled-axis rounding matches pi's
`Math.round`.
- GIF/WebP fall back to `image`'s full decode. Progressive JPEG, interlaced
PNG, 16-bit PNG, CMYK JPEG, and EXIF-rotated JPEG all decode.

### Retained cost in a session

The encoded image stays in the transcript until compaction, so the per-session
cost is the base64 payload, not the transient. `--stress-session --stress-tool
image` keeps results, and reports the retained bytes:

| images retained | retained | peak RSS |
| --- | --- | --- |
| 10 | 2.7 MB | 31 MB |
| 20 | 5.4 MB (~261 KB each) | 31 MB |

With the 1024 / 1 MiB target, twenty images hold ~5 MB, not ~27 MB. CI gates
both the transient (`--max-mb 60`) and the retained run (`--max-mb 60`).

### Concurrency

The gateway hosts many units in one process, so image decodes are capped by a
process-global gate (default 2, `PIPELETS_IMAGE_CONCURRENCY`), bounding
aggregate image memory while a decode is in flight. `malloc_trim(0)` runs after
each decode so the transient is not left resident.

## Session store

`pi-session` streams a session file line by line (`BufReader`); the raw file is
never held as one string. A 3.2 MB session reads losslessly at **10.3 MB peak
RSS** (pi-node holds ~124 MB for the same session).

```bash
cargo run -p pi-session --example read_session -- path/to/session.jsonl
```

## Lean release profile

The release profile is tuned for size and speed:

```toml
lto = "fat"
opt-level = 2
codegen-units = 1
panic = "abort"
strip = true
```

Fat LTO + `opt-level=2` measured smaller and not slower than thin/3 (binary
~8.4 MB, idle 4.3 MB). CI's `memory-gate` job installs pi-node and the reference
Rust port and re-runs `scripts/mem_gate.py` on every change.
