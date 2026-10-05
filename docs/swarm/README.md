# pi-native swarm — unit guide

You are one **role unit** in a pi-native swarm. This file lives in the repo so
you can read it inside the workspace jail. The human talks to the **conductor**
unit; you receive work from the conductor and coordinate through Linear.

## The board is the shared channel

Team **VED**, project **pi native runtime** (`4d5e47500fa6`). Use the `linear`
CLI (credentials are already configured).

- List work: `linear issue mine --team VED --project 4d5e47500fa6 --all-states --json`
- Read before you work: `linear issue comment list <ISSUE>` — another unit may
  have left notes or partial work.
- Post updates / ask / hand off:
  `linear issue comment add <ISSUE> --body "..."` (prefer `--body-file`).
- Keep the issue state current: `linear issue update <ISSUE> --state "In Progress"`
  / `"Done"`.
- **Sign every comment with your role**, e.g. `- [coder]`.

## Roles and routing

| Unit | Owns | May edit code |
| --- | --- | --- |
| `planner` | backlog, prioritisation | no (analysis only) |
| `researcher` | `pi-providers`, `pi-cache`, `pi-net`, `pi-plugins` | yes |
| `coder` | `pi-gateway`, `pi-host`, `pi-triggers`, `pi-agent`, `pi-session`, `pi-rpc` | yes |
| `reviewer` | review of `pi-rpc` / `pi-agent` | no (analysis only) |
| `tester` | tests and gates | yes (tests only) |
| `synthesizer` | status, docs | docs only |
| `conductor` | planning + dispatch | no |

A **read-only** unit that receives a code card must bounce it back on the issue
with the reason instead of declining silently.

## Working on code

- Code cards run in an **isolated worktree**: `python3 scripts/swarm_worktree.py
  create <ID>` prints a path (e.g. `/home/vedaru/Projects/pi-native-ws/<ID>`) on
  branch `swarm/<ID>`. Work there, commit there, never the main tree.
- **One editor at a time** — only one unit should run a heavy `cargo` build.
- Gates before Done: `cargo fmt --all`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `cargo test --workspace`.
- **Do not push.** The synthesizer pushes.
- Scratch files: write to `/tmp` (an allowed write root); **never** leave temp
  files in the repo.

## Patterns

`python3 scripts/swarm_run.py --pattern concurrent|sequential|moa --task "..."` runs
an orchestration across units (the conductor uses this).
