# Providers, prompt cache, and wire parity

The native runtime MUST be indistinguishable from pi at the provider API for an
equivalent session. Any change that alters outbound request bytes or lowers the
prompt-cache hit rate versus pi is a regression that blocks merge and release
(release gate VED-315, gated on VED-313 wire parity and VED-314 cache hit rate).

## Provider selection

`--provider` is fixed at startup:

```bash
# OpenAI Chat Completions
pipelets --serve --provider openai-completions --base-url https://api.deepseek.com \
  --model deepseek-flash --thinking-format deepseek

# OpenAI Responses
pipelets --serve --provider openai-responses --base-url https://api.openai.com/v1 --model gpt-4o
```

Flags: `--provider`, `--base-url` (or `OPENAI_BASE_URL`), `--api-key` (or
`OPENAI_API_KEY`), `--model`, `--max-tokens`, `--thinking-format none|deepseek`,
`--context-window <tokens>` (compaction; 0 disables). `--thinking-format`
controls how the reasoning effort is encoded on the wire.

## Prompt-cache primitives (`pi-cache`)

`pi-cache` ports pi's cache primitives and is unit-tested against pi's semantics:

- retention resolution and Anthropic `cache_control` markers,
- OpenAI `prompt_cache_key` clamping and wiring,
- cache-miss detection and waste accounting,
- cache-warming delay, replayability, and economics.

```bash
cargo run -q -p pipelets -- cache-control --retention long
# {"cache_control":{"ttl":"1h","type":"ephemeral"},"retention":"long"}

cargo run -q -p pipelets -- prompt-cache-key --session-id sess-123 --responses
# {"prompt_cache_key":"sess-123","retention":"short"}
```

Because outbound requests are byte-identical to pi, the prompt cache behaves the
same: on DeepSeek the system+tools prefix (1,408 tokens) is cached and a
multi-turn session runs at ~86% hit per turn — pi's numbers. Measured
2026-10-06: 86.6 / 86.0 / 85.3% over three warm turns. A swarm unit measures the
same (85.8 / 92.2 / 89.7%): the `<swarm>` prompt section is a stable prefix,
cached like the rest of system+tools, and a peer shout only extends the
uncached suffix of the current turn.

## Wire-parity harness

`scripts/harness/capture.mjs` captures pi's real outgoing request by injecting a
fake `fetch` into pi's provider, then writes canonical JSON fixtures. The Rust
builder in `pi-providers` is tested against those fixtures.

```bash
./scripts/parity-check.sh
```

Scenarios live in `scripts/harness/scenarios/` and select a `provider`
(`openai-completions`, `openai-responses`). Fixtures live in each crate's
`tests/fixtures/`. JSON key order is canonicalized so only semantic changes
count. `PI_AI_DIST` overrides the installed pi-ai path.

## Transport (`pi-net`)

Every provider call is bounded so a stalled provider cannot wedge a unit:
`CONNECT_TIMEOUT=10s`, `RESPONSE_TIMEOUT=60s`, `REQUEST_TIMEOUT=300s`.
