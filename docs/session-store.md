# Session store (`pi-session`)

`pi-session` parses and writes pi's JSONL session format. Entries keep their
common fields typed (`type`/`id`/`parentId`/`timestamp`) and preserve all other
fields verbatim, so new entry kinds round-trip without code changes.

- Loading is streamed line by line (`BufReader`); the raw file is never held as
  one string.
- `build_context` walks the leaf branch, applies the latest compaction, and
  applies `context_edit` entries.
- Measured: a 3.2 MB session reads losslessly at **10.3 MB peak RSS** (the same
  session costs pi ~124 MB total).

Without `--session`, a run creates a session file under the pi agent directory
(`$PI_CODING_AGENT_DIR`, else `~/.pi/agent`) in pi's layout:
`sessions/--<encoded-cwd>--/<timestamp>_<id>.jsonl`. That is the same layout pi
and pi-web read, so their session browsers can list and resume native sessions.

```bash
cargo run -p pi-session --example read_session -- path/to/session.jsonl
```

The RPC surface over sessions is documented in [rpc-protocol.md](rpc-protocol.md).
