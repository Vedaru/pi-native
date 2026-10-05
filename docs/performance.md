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
| shipped runtime | one **12.1 MB** binary (4.6 MB tarball) | Node + `node_modules` |
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

An image read is **decoded at a reduction that covers the target**, so the
full-size source bitmap is never materialised, then resized with
`fast_image_resize` and encoded under the inline limit. `--stress-tool image`
decodes and resizes a 2100x2100 screenshot-like PNG per call:

| | Value |
| --- | --- |
| 50 resizes | 5.9 s, **50 MB** peak, 5.9 s CPU |
| one 2100x2100 -> 2000x2000 | ~23 ms |

For a **7000x7000** source (target 2000x2000):

| | PNG | JPEG |
| --- | --- | --- |
| decode stage | 2333x2333, **20 MB** | 3500x3500, **70 MB** |
| full read | **77 MB** | **138 MB** |
| old full-decode path | ~370 MB | ~370 MB |

How it decodes:

- **JPEG** uses `jpeg-decoder`'s IDCT scaling (1/8, 1/4, 1/2): a 7000px JPEG is
decoded at 1/2, never at full size. The scale steps are powers of two, so 1/2
is the smallest that still covers 2000; turbojpeg's finer factors would shrink
this further at the cost of a C dependency.
- **PNG** is decoded scanline by scanline and box-downsampled on the fly, so a
7000px PNG never allocates more than the reduced bitmap.
- GIF/WebP fall back to `image`'s full decode (small in practice), and the read
is header-first so the dimensions are known before any pixels are touched.

### Retained cost in a session

The encoded image stays in the transcript until compaction, so the per-session
cost is the base64 payload, not the transient. `--stress-session --stress-tool
image` keeps results, and reports the retained bytes:

| images retained | retained | peak RSS |
| --- | --- | --- |
| 10 | 13.4 MB | 64 MB |
| 20 | 26.7 MB (~1.3 MB each) | 76 MB |

A real screenshot can encode to ~2.4 MB, so ten of them hold ~24 MB until
compaction. CI gates both the transient (`--max-mb 80`) and the retained run
(`--max-mb 120`).

### Concurrency

The gateway hosts many units in one process, so image decodes are capped by a
process-global gate (default 2, `PIPELETS_IMAGE_CONCURRENCY`), bounding
aggregate image memory. After a decode, `malloc_trim(0)` returns what glibc
would otherwise keep (a 2100x2100 read: 12 MB -> 4.7 MB). The freed transient
sits below the retained base64, so the unit can still rest at ~12 MB after an
image read; that residue is allocator-held free, not a leak.

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
