# Unit gateway and triggers

`--gateway` serves the unit host over HTTP + SSE so a web UI or remote client can
attach to long-lived agents. Sessions are addressed by id, not by a file
descriptor; any number of clients can attach, and an idle unit releases its
in-memory agent until the next command.

```bash
pipelets --gateway --gateway-addr 127.0.0.1:30142 \
  --provider openai-completions --base-url … --model … --api-key …
```

## Routes

| Method | Path | Meaning |
| --- | --- | --- |
| `GET` | `/sessions` | List session ids |
| `GET` | `/swarm` | Status snapshot of every unit (multi-agent dashboard) |
| `GET` | `/runs` | Run receipts (model, tokens, cost, outcome, files changed) |
| `POST` | `/sessions` | Open/create a session (`{"sessionPath"?: …, "cwd"?: …}`) |
| `GET` | `/sessions/:id` | Resolved state |
| `DELETE` | `/sessions/:id` | Remove a unit |
| `GET` | `/sessions/:id/commands` | Extension slash commands |
| `POST` | `/sessions/:id/title` | Generate a session title from the transcript |
| `GET` | `/sessions/:id/events` | SSE stream (replay + live) |
| `POST` | `/sessions/:id/commands` | Send a command (`prompt`, `steer`, `abort`, …) |
| `POST` | `/sessions/:id/ui_response` | Answer a `ui_request` |
| `GET` | `/units/:id/messages` | Poll a unit's mailbox |
| `POST` | `/units/:id/messages` | Send a unit-to-unit message |
| `GET` | `/cron` | List runtime trigger jobs |
| `POST` | `/cron` | Add/replace a job (`{id, interval_secs\|cron, prompt, target?}`) |
| `DELETE` | `/cron/:id` | Remove a job |

By default the SSE stream carries the native event envelope; add `?format=pi`
to receive pi's canonical events (`agent_start`, `message_update`,
`tool_execution_*`, `agent_settled`) through `pi_rpc::PiEventAdapter`.

Units run tools without asking, matching pi. Binding a non-loopback address
without `--gateway-token` is refused at startup; see
[the web integration](../integrations/pi-web/README.md) for the threat model.

## Triggers

`--triggers <file>` (with `--gateway`) fires scheduled prompts into long-lived
sessions. Each trigger has a stable session, so context accumulates across runs;
run records are appended to `.pipelets/trigger-runs.jsonl` (or
`--trigger-runs <path>`) and are the dedupe/idempotency layer across restarts.
Every finished run also leaves a **receipt** (model, tokens, cost, exit code,
files changed, diff, outcome) in a sibling `.receipts.jsonl` file, served over
`GET /runs`.

```json
[
  {"id":"hourly-sweep","interval_secs":3600,"prompt":"check the queue","dedupe_window_secs":3600},
  {"id":"standup","cron":"0 9 * * 1-5","prompt":"summarize yesterday","max_runs_per_window":1,"budget_window_secs":86400,
   "model":"deepseek-flash",
   "budget":{"max_tokens":200000,"max_cost_micros":50000,"max_seconds":600},
   "agent_budget":{"max_cost_micros":5000000}}
]
```

```bash
pipelets --gateway --triggers ./triggers.json --trigger-interval 1 \
  --run-price 270000,1100000,27000,270000 \
  --provider openai-completions --base-url … --model … --api-key …
```

Schedules are `interval_secs` or a 5-field `cron` (`min hour day month weekday`).
A `budget` bounds each run (tokens, spend in micro-units, wall-clock) and an
`agent_budget` bounds the cumulative spend of one trigger. `--run-price`
supplies `input,output,cache_read,cache_write` rates in micro-units per million
tokens so token usage becomes spend. When a run crosses a budget it is aborted
and its changes are parked in a `git stash` (or restored, on request); the
receipt records the breach and disposition.

A trigger with a `target` delivers into that existing session instead of owning
one — this is how a conductor unit schedules its own tick. The runtime tick fires
the due triggers, then **releases the host lock before waiting** on each run, so
a run that calls back into the gateway (the conductor's `GET /swarm`) cannot
deadlock it (VED-389).
