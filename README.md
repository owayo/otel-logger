<h1 align="center">otel-logger</h1>

<p align="center">
  <strong>OTLP receiver that logs Claude Code / Codex telemetry to stdout and JSON Lines</strong>
</p>

<p align="center">
  <a href="https://github.com/owayo/otel-logger/actions/workflows/ci.yml">
    <img alt="CI" src="https://github.com/owayo/otel-logger/actions/workflows/ci.yml/badge.svg?branch=main">
  </a>
  <a href="https://github.com/owayo/otel-logger/releases/latest">
    <img alt="Version" src="https://img.shields.io/github/v/release/owayo/otel-logger">
  </a>
  <a href="LICENSE">
    <img alt="License" src="https://img.shields.io/github/license/owayo/otel-logger">
  </a>
</p>

<p align="center">
  English | <a href="README.ja.md">日本語</a>
</p>

---

## Overview

`otel-logger` is a tiny Rust OTLP receiver designed to sit next to AI coding
agents — **Claude Code** and **OpenAI Codex CLI** — while they run inside CI
containers. It accepts OTLP/gRPC on `:4317` and OTLP/HTTP on `:4318`, decodes
traces, metrics, and logs, and writes them in two ways:

- **stdout**: human-readable, color-coded one-liner per record (great for CI logs).
- **JSON Lines** (`--log-file`): lossless, schema-preserving for offline analysis.

It does **not** forward to Jaeger/Honeycomb/etc. — the goal is to capture what
the agent emits during a CI job and surface it where developers already look.

## Features

- OTLP/gRPC (4317) and OTLP/HTTP (4318) on the same process
- Accepts both `application/x-protobuf` and `application/json` on HTTP
- Pretty stdout output with severity-based color (auto-disabled when redirected or `NO_COLOR` is set)
  - Hardens against terminal escape injection: ANSI escapes and other C0/C1 control characters in incoming payloads, including dynamic labels and attribute keys, are escaped before reaching the terminal (JSONL output stays lossless)
- JSON Lines persistence, `fsync`'d on graceful shutdown
  - Persistence failures surface as HTTP 5xx / gRPC `Status::internal` so OTLP exporters can retry instead of silently dropping payloads
  - Usage totals are updated only after JSONL persistence succeeds, so retried batches are not counted twice
  - Each batch is `flush`'d to the kernel before ACK so an unexpected crash never leaves the last write trapped in `BufWriter`'s in-memory buffer
  - gRPC/HTTP raise their per-request decode limit to 32 MiB (above `tonic`'s 4 MiB / `axum`'s 2 MiB defaults) so large batches are persisted instead of being permanently rejected with `RESOURCE_EXHAUSTED` / `413` that exporter retries cannot recover from
- Cumulative `/stats` and `--summary` usage totals for Claude/Codex with
  de-duplication between logs and metrics
  - Recognises every Codex binary via `service.name` (TUI `codex_cli_rs`, Exec `codex_exec`, Apps Server `codex-app-server` from Codex 0.140.0+) so Apps-Server-only deployments — which emit logs/traces but no `codex.turn.*` metrics — are still aggregated
  - For Codex, SSE `response.completed` logs are the preferred token source when present; WebSocket `response.completed` events without usage and trace-span usage mirrors such as `session_task.turn` / `session_task.review` are not counted as separate usage
  - For Codex, pending tokens recorded under `effort=unknown` are tracked per `conversation.id` so a delayed `codex.conversation_starts` only moves the matching conversation's tokens — never another concurrent conversation's. If the late `codex.conversation_starts` arrives with a non-default `provider_name` (Azure or other openai-compatible endpoints), pending entries keyed under the SSE-time default provider are still located by `(model, conversation_id)` and moved from their original provider bucket to the session's provider bucket
  - `handle_responses` spans also respect `conversation.id` when re-deriving `effort`, so a span for one conversation never overwrites another conversation's session
  - Non-finite or out-of-range numeric attributes and metric values (`NaN`, `±Infinity`, huge `double`s) on tokens, durations and cost are rejected at parse time so untrusted telemetry sources cannot poison cumulative counters with saturated `i64::MAX` / `u64::MAX` or `cost_usd=inf`
  - Cumulative counters use saturating arithmetic, so repeated extreme batches cannot wrap token totals or turn cost into `Infinity`
- Graceful shutdown on SIGINT and SIGTERM (no lost batch under `docker stop`)
- Single static-ish binary (~7 MB stripped) and a distroless container image
- Examples for Docker Compose, GitHub Actions, and GitLab CI

## Requirements

- **Runtime OS**: Linux, macOS
- **Rust**: 1.88+ (only when building from source — `tonic 0.14` requires it; uses edition 2024). The bundled Dockerfile uses `rust:1.90` to stay ahead of the floor.

## Installation

### Homebrew (macOS / Linux)

```bash
brew install owayo/otel-logger/otel-logger
```

The tap ships pre-built bottles for `arm64_sonoma`, `sonoma`, and
`x86_64_linux`; other platforms fall back to building from source via
`cargo install` (Rust 1.88+ is pulled in via `depends_on "rust"`).

### From source

```bash
cargo install --path .
```

### From a release

Download the binary for your platform from
[Releases](https://github.com/owayo/otel-logger/releases) and place it in
`$PATH`. Available artifacts:
`otel-logger-{aarch64,x86_64}-apple-darwin.tar.gz`,
`otel-logger-{x86_64,aarch64}-unknown-linux-gnu.tar.gz`,
`otel-logger-x86_64-unknown-linux-musl.tar.gz` (static build for distroless /
Alpine), and `otel-logger-x86_64-pc-windows-msvc.zip`.

### Docker

```bash
docker build -t otel-logger:dev .
docker run --rm -p 4317:4317 -p 4318:4318 otel-logger:dev
```

## Usage

```bash
otel-logger [OPTIONS]
```

### Options

| Option         | Short | Default          | Env                       | Description                                              |
|----------------|-------|------------------|---------------------------|----------------------------------------------------------|
| `--config`     |       | (auto)           | `OTEL_LOGGER_CONFIG`      | Path to a TOML config file (see below)                   |
| `--grpc-addr`  |       | `0.0.0.0:4317`   | `OTEL_LOGGER_GRPC_ADDR`   | gRPC bind address (OTLP/gRPC)                            |
| `--http-addr`  |       | `0.0.0.0:4318`   | `OTEL_LOGGER_HTTP_ADDR`   | HTTP bind address (OTLP/HTTP, both protobuf and JSON)    |
| `--log-file`   |       | (none)           | `OTEL_LOGGER_LOG_FILE`    | Append received telemetry as JSON Lines (mutually exclusive with `--log-dir`) |
| `--log-dir`    |       | (none)           | `OTEL_LOGGER_LOG_DIR`     | Write daily-rotated JSONL into this directory: `otel-logger.YYYY-MM-DD` (local time) |
| `--log-keep-days` |    | `10`             | `OTEL_LOGGER_LOG_KEEP_DAYS` | Days of rotated JSONL to keep when `--log-dir` is used (`0` is clamped to a 1-day minimum) |
| `--no-stdout`  |       | `false`          | `OTEL_LOGGER_NO_STDOUT`   | Suppress the human-readable stdout stream                |
| `--summary`    |       | `false`          | `OTEL_LOGGER_SUMMARY`     | Append cumulative usage summary when usage totals change |
| `--color`      |       | `auto`           | `OTEL_LOGGER_COLOR`       | `auto` / `always` / `never` (honors `NO_COLOR`)          |
| `--dry-run`    | `-n`  | `false`          |                           | Validate startup, including simultaneous listener bind, then exit |
| `--help`       | `-h`  |                  |                           | Show help                                                |
| `--version`    | `-V`  |                  |                           | Show version                                             |

### Config file (`~/.config/otel-logger/config.toml`)

`otel-logger` reads `$XDG_CONFIG_HOME/otel-logger/config.toml` on startup
(falling back to `~/.config/otel-logger/config.toml`). Use `--config <PATH>`
to point at a different file. Every key is optional; missing keys fall back
to the built-in default.

Leading `~` / `~/` is expanded to `$HOME` for `--config`, `otel-logger init
--path`, `log-file`, and `log-dir`, including values supplied through
environment variables or the config file.

**Precedence** (highest wins): CLI flag > environment variable > config file > default.
For the mutually exclusive log sinks, this precedence also applies across
`log-file` and `log-dir`: specifying `--log-file` ignores a configured
`log-dir`, and specifying `--log-dir` ignores a configured `log-file`.

When `log-dir` is used, retention cleanup only removes daily rotated files
named `otel-logger.YYYY-MM-DD`. Other files in the same directory, such as
`otel-logger.pid`, `otel-logger.stderr.log`, or a standalone
`otel-logger.jsonl`, are left untouched.

Generate a fully-commented starter file with the bundled command:

```bash
otel-logger init                    # → ~/.config/otel-logger/config.toml
otel-logger init -p /etc/foo.toml   # → custom path
otel-logger init -f                 # overwrite an existing file
```

The generated file looks like:

```toml
# ~/.config/otel-logger/config.toml
log-file = "/var/log/otel-logger/otel-logger.jsonl"
# Or, daily-rotated output (mutually exclusive with `log-file`):
# log-dir = "/var/log/otel-logger"
# log-keep-days = 10                 # default: 10
no-stdout = false
summary = false
color = "auto"  # "auto" | "always" | "never"
# grpc-addr = "0.0.0.0:4317"
# http-addr = "0.0.0.0:4318"
```

Note: TOML paths are not generally shell-expanded. A leading `~` / `~/` is
expanded for the path settings listed above, but embedded environment variables
such as `$HOME/logs` are not expanded; write absolute paths for those cases.

Internal logs (the receiver's own diagnostics) go to **stderr** and respect
`OTEL_LOGGER_LOG=debug` (`tracing-subscriber` env filter syntax).

### Examples

```bash
# Listen on the standard OTLP ports and write JSONL to disk
otel-logger --log-file ./otel.jsonl

# Run only the HTTP listener (point gRPC at an unused address)
otel-logger --grpc-addr 127.0.0.1:0 --http-addr 0.0.0.0:4318

# Smoke test from CI
otel-logger --dry-run
```

### Usage summary

`--summary` appends cumulative Claude/Codex usage totals to stdout whenever the
receiver ingests a new usage sample. `GET /stats` always returns the same
snapshot as JSON. Claude API request logs are treated as the preferred source
for token/cost usage when both logs and metrics are present, because logs arrive
per request and can include usage that has not yet been exported as metrics.
Matching metrics are de-duplicated instead of being added twice.

## Sending telemetry from Claude Code

Claude Code's telemetry contract is environment-variable driven.
See [Anthropic monitoring docs](https://code.claude.com/docs/en/monitoring-usage).
The simplest way to apply the env vars in every session is the `env` block of
`~/.claude/settings.json` (or `.claude/settings.json` per-project):

```jsonc
{
  "env": {
    "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
    "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA": "1",
    "OTEL_LOGS_EXPORTER": "otlp",
    "OTEL_METRICS_EXPORTER": "otlp",
    "OTEL_TRACES_EXPORTER": "otlp",
    "OTEL_EXPORTER_OTLP_PROTOCOL": "http/protobuf",
    "OTEL_EXPORTER_OTLP_ENDPOINT": "http://localhost:4318",
    "OTEL_EXPORTER_OTLP_TIMEOUT": "5000",
    "OTEL_RESOURCE_ATTRIBUTES": "service.name=claude-code,deployment.environment=local"
  }
}
```

When running under Docker Compose / CI alongside the receiver container,
swap `localhost` for the service name (`otel-logger`). The same variables can
also be `export`-ed in a shell if you prefer not to use `settings.json`.

## Sending telemetry from OpenAI Codex CLI

Codex contracts on `config.toml` rather than environment variables.
See [Codex config reference](https://developers.openai.com/codex/config-reference).

```toml
# $CODEX_HOME/config.toml
[otel]
environment = "local"
log_user_prompt = false # do not transmit prompt content
exporter = { otlp-http = { endpoint = "http://localhost:4318/v1/logs", protocol = "binary", headers = {} } }
metrics_exporter = { otlp-http = { endpoint = "http://localhost:4318/v1/metrics", protocol = "binary", headers = {} } }
trace_exporter = { otlp-http = { endpoint = "http://localhost:4318/v1/traces", protocol = "binary", headers = {} } }
```

When running under Docker Compose / CI alongside the receiver container,
swap `localhost` for the service name (`otel-logger`).

A working sample lives at [`codex-config/config.toml`](codex-config/config.toml).

## Docker Compose

The repo ships with a sample [`compose.yaml`](compose.yaml) that runs
`otel-logger` as a sidecar and shows two consumer containers (Claude Code and
Codex):

```bash
docker compose up otel-logger
docker compose run --rm claude-code-sample
docker compose run --rm codex-sample
```

The receiver logs go to `docker compose logs otel-logger`, and the lossless
JSONL stream lands in `./data/otel-logger.jsonl`.

## CI examples

### GitHub Actions

```yaml
jobs:
  ai-job:
    runs-on: ubuntu-latest
    services:
      otel-logger:
        image: ghcr.io/owayo/otel-logger:latest
        ports:
          - 4317:4317
          - 4318:4318
        options: >-
          --health-cmd "/usr/local/bin/otel-logger --dry-run"
          --health-interval 10s
          --health-timeout 3s
          --health-retries 3
    env:
      CLAUDE_CODE_ENABLE_TELEMETRY: "1"
      OTEL_LOGS_EXPORTER: otlp
      OTEL_METRICS_EXPORTER: otlp
      OTEL_TRACES_EXPORTER: otlp
      CLAUDE_CODE_ENHANCED_TELEMETRY_BETA: "1"
      OTEL_EXPORTER_OTLP_PROTOCOL: http/protobuf
      OTEL_EXPORTER_OTLP_ENDPOINT: http://localhost:4318
      OTEL_RESOURCE_ATTRIBUTES: "service.name=claude-code,deployment.environment=gha"
    steps:
      - uses: actions/checkout@v4
      - run: npm install -g @anthropic-ai/claude-code
      - run: claude --print "your prompt here"
```

### GitLab CI

```yaml
ai-job:
  image: node:20-bookworm-slim
  services:
    - name: ghcr.io/owayo/otel-logger:latest
      alias: otel-logger
  variables:
    CLAUDE_CODE_ENABLE_TELEMETRY: "1"
    OTEL_LOGS_EXPORTER: otlp
    OTEL_METRICS_EXPORTER: otlp
    OTEL_TRACES_EXPORTER: otlp
    CLAUDE_CODE_ENHANCED_TELEMETRY_BETA: "1"
    OTEL_EXPORTER_OTLP_PROTOCOL: http/protobuf
    OTEL_EXPORTER_OTLP_ENDPOINT: http://otel-logger:4318
    OTEL_RESOURCE_ATTRIBUTES: "service.name=claude-code,deployment.environment=gitlab"
  script:
    - npm install -g @anthropic-ai/claude-code
    - claude --print "your prompt here"
```

## Cumulative usage stats

`otel-logger` aggregates token / cost / duration usage from both agents and
exposes the running totals two ways. Claude usage is metrics-first for
tokens/cost and log-assisted for request count/duration. Codex token usage is
deduplicated across the two shapes Codex emits: observed Codex 0.129/0.130
local and CI logs provide the most complete token counters on
`codex.sse_event` / `response.completed`, and `codex.turn.token_usage`
metrics remain the fallback when they are the first or only token source
observed.

| Agent       | tokens & cost                                            | request_count                       | duration                           | metadata                                                         |
|-------------|----------------------------------------------------------|-------------------------------------|------------------------------------|------------------------------------------------------------------|
| claude-code | metrics `claude_code.token.usage` + `claude_code.cost.usage` | log `claude_code.api_request`         | log `claude_code.api_request.duration_ms` | —                                                                |
| codex       | log `codex.sse_event` / `response.completed` or fallback metric `codex.turn.token_usage` (Histogram, `total` ignored) | metric `codex.conversation.turn.count` | metric `codex.turn.e2e_duration_ms` | log/span event `codex.conversation_starts` for `provider`/`effort` |

Anthropic logs strip variant suffixes from `model` (e.g. `claude-opus-4-7`)
while metrics carry the full name (`claude-opus-4-7[1m]`). The aggregator
canonicalizes log-side bare names to whichever full name was last seen on a
metric, so 1M and standard variants do not fragment into separate buckets.
Only `aggregationTemporality=DELTA` is honored; cumulative points are
dropped with a warning.

Codex emits both SSE completion logs and turn token metrics for the same
usage. `otel-logger` accepts the first token source observed for each model
and ignores the other source for that model's token counters to avoid
double-counting.
`tool_token_count` is not added because it overlaps the other token classes.
Real logs also include tool-only `response.completed` events where
`input_token_count == tool_token_count` and output/cache/reasoning are all
zero; Codex excludes those from turn metrics and `handle_responses` span
usage, so `otel-logger` does not count them as token usage.
`session_task.turn` / `session_task.review` spans can also carry
`codex.turn.token_usage.*`; those are mirrors of the same usage, so trace
spans are not used as token sources.
If Codex token logs arrive before `conversation_starts`, the temporary
`effort=unknown` bucket is folded into the later provider/model/effort bucket
when the session metadata arrives.
When `conversation.id` is present on an SSE completion log, the aggregator
uses only the matching `codex.conversation_starts` metadata — it never falls
back to the last observed session of a different `conversation.id`. If the
matching `conversation_starts` has not been seen yet, the entry lands in an
`effort=unknown` bucket and is merged into the correct effort bucket once the
metadata catches up. This keeps interleaved or long-running Codex
conversations from moving token usage into the wrong effort bucket.

### `GET /stats` (always on)

```bash
$ curl -s http://localhost:4318/stats | jq
{
  "started_at": "2026-05-08T...",
  "last_updated": "2026-05-08T...",
  "agents": {
    "claude-code": {
      "total": {
        "request_count": 81,
        "input_tokens": 65509,
        "output_tokens": 85207,
        "cache_read_tokens": 8924351,
        "cache_creation_tokens": 724182,
        "reasoning_output_tokens": 0,
        "cost_usd": 10.871609,
        "duration_ms": 1262136
      },
      "buckets": {
        "anthropic/claude-opus-4-7[1m]/max": {
          "provider": "anthropic",
          "model": "claude-opus-4-7[1m]",
          "effort": "max",
          "request_count": 74,
          "input_tokens": 1253,
          "output_tokens": 82097,
          "cache_read_tokens": 8924351,
          "cache_creation_tokens": 671142,
          "reasoning_output_tokens": 0,
          "cost_usd": 10.715503,
          "duration_ms": 1224418
        }
      }
    },
    "codex": {
      "total": { "request_count": 8, "input_tokens": 2469774, "reasoning_output_tokens": 12744, ... },
      "buckets": {
        "OpenAI/gpt-5.5/xhigh":     { "provider": "OpenAI", "model": "gpt-5.5",     "effort": "xhigh", ... },
        "OpenAI/gpt-5.4-mini/low":  { "provider": "OpenAI", "model": "gpt-5.4-mini", "effort": "low",   ... }
      }
    }
  }
}
```

Bucket keys are formatted as `provider/model/effort`. Codex's `cost_usd` is
always `0` because the OpenAI/ChatGPT side does not emit cost. This endpoint
is always available — no flag required.

### `--summary` (stdout, opt-in)

When `--summary` (or `OTEL_LOGGER_SUMMARY=1` / `summary = true` in the config)
is enabled, otel-logger appends a `[stats:<agent>]` block right after every
batch that changes cumulative usage totals:

```
[stats:claude-code] requests=81 input=65509 output=85207 cache_read=8924351 cache_create=724182 reasoning=0 cost=$10.8716 duration=1262.400s since=2026-05-08T...
        breakdown provider=anthropic model=claude-opus-4-7[1m] effort=max: requests=74 input=1253 output=82097 cache_read=8924351 cache_create=671142 reasoning=0 cost=$10.7155 duration=1234.560s
        breakdown provider=anthropic model=claude-haiku-4-5-20251001 effort=unknown: requests=7 input=64256 output=3110 cache_read=0 cache_create=53040 reasoning=0 cost=$0.1561 duration=27.840s
```

Counters are process-lifetime cumulative; restarting otel-logger resets them.

## Output format

### stdout (pretty)

Dynamic payload fields such as service names, span names, metric names, severity text, and attribute keys are escaped before printing.

```
[trace]  2026-05-07T22:01:14.123Z service=claude-code scope=anthropic.claude_code span=tool.call dur=812ms status=OK trace=4d2... span_id=8ab...
        attrs: {tool.name=Bash, exit_code=0}
[log]    2026-05-07T22:01:14.456Z service=claude-code scope=anthropic.claude_code severity=INFO body="ran command"
[metric] service=claude-code scope=anthropic.claude_code name=claude_code.tokens.input sum=[1234 {model=claude-sonnet-4-6}]
```

### JSON Lines (`--log-file`)

Each line is a JSON object with the original protobuf payload preserved
(keys are in OTLP/JSON camelCase):

```json
{"kind":"traces","resourceSpans":[{"resource":{"attributes":[…]},"scopeSpans":[…]}]}
{"kind":"metrics","resourceMetrics":[…]}
{"kind":"logs","resourceLogs":[…]}
```

Process the file with `jq` or feed it into your warehouse of choice.

## Development

```bash
make build      # cargo build
make test       # cargo test
make check      # cargo check --all-targets
make clippy     # cargo clippy --all-targets -- -D warnings
make fmt        # cargo fmt
make run        # run with --log-file ./otel-logger.jsonl
make docker     # build the container image
```

## How it works

- `tonic` exposes the three OTLP gRPC services (`TraceService`, `MetricsService`, `LogsService`) on port 4317.
- `axum` serves `/v1/traces`, `/v1/metrics`, `/v1/logs` on port 4318 and accepts both `application/x-protobuf` (decoded with `prost`) and `application/json` (decoded via `serde`). The `Content-Type` media type is matched case-insensitively (RFC 9110), so values such as `Application/X-Protobuf; charset=utf-8` are accepted.
- Both transports raise their per-request decode limit to 32 MiB (`OTLP_MAX_REQUEST_BYTES`) so a large batch is never permanently rejected by the 4 MiB / 2 MiB transport defaults.
- Both transports converge on a shared `Sink` that writes pretty stdout and lossless JSONL.
- `tokio_util::sync::CancellationToken` plus a `tokio::select!` that listens for SIGINT/SIGTERM gives a clean shutdown; gRPC/HTTP tasks are awaited before the final JSONL flush so the trailing batch never disappears.

## License

[MIT](LICENSE)
