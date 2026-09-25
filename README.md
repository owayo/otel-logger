<h1 align="center">otel-logger</h1>

<p align="center">
  OTLP receiver that logs Claude Code / Codex telemetry to stdout and JSON Lines
</p>

<!-- standard:badges:start -->
<h3 align="center">Supported Platforms</h3>

<p align="center">
  <img src="https://img.shields.io/badge/Linux-FCC624?logo=linux&amp;logoColor=black" alt="Linux">
  <img src="https://img.shields.io/badge/macOS-000000?logo=apple&amp;logoColor=white" alt="macOS">
  <img src="https://img.shields.io/badge/Windows-0078D6" alt="Windows">
</p>

<p align="center">
  <a href="https://github.com/owayo/otel-logger/actions/workflows/ci.yml"><img src="https://github.com/owayo/otel-logger/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/owayo/otel-logger/releases/latest"><img src="https://img.shields.io/github/v/release/owayo/otel-logger" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/owayo/otel-logger" alt="License"></a>
</p>

<p align="center">
  <a href="README.md">English</a> |
  <a href="README.ja.md">日本語</a>
</p>
<!-- standard:badges:end -->

---

`otel-logger` is a tiny Rust OTLP receiver designed to sit next to AI coding agents — **Claude Code** and **OpenAI Codex CLI** — while they run inside CI containers. It accepts OTLP/gRPC on `:4317` and OTLP/HTTP on `:4318`, decodes traces, metrics, and logs, and writes them in two ways:

- **stdout**: human-readable, color-coded one-liner per record (great for CI logs).
- **JSON Lines** (`--log-file` / `--log-dir`): lossless, schema-preserving for offline analysis, either as one append-only file or daily-rotated files.

By default it does **not** forward to Jaeger/Honeycomb/etc. — the goal is to capture what the agent emits during a CI job and surface it where developers already look. Optional OTLP proxy routes can forward the persisted payloads to upstream collectors.

## Features

- **Both OTLP transports in one process**: OTLP/gRPC (4317) and OTLP/HTTP (4318); HTTP accepts both `application/x-protobuf` and `application/json`, gzip-compressed or not
- **Readable stdout**: severity-based colors (turned off when redirected or when `NO_COLOR` is set), with control characters in incoming payloads escaped before they reach the terminal
- **Lossless JSON Lines**: one append-only file or daily-rotated files, `fsync`'d on graceful shutdown; a failed write returns HTTP `503` / gRPC `Unavailable` so exporters retry instead of dropping the batch
- **Usage totals for Claude Code and Codex**: cumulative tokens, cost and duration per provider/model/effort via `GET /stats` and `--summary`, de-duplicated between logs and metrics
- **OTLP proxy forwarding**: optional routes that forward Claude Code and Codex payloads to separate upstream collectors after they are persisted
- **Graceful shutdown**: SIGINT and SIGTERM keep the last batch (nothing is lost under `docker stop`), bounded by a 10-second grace period
- **Private files**: JSONL files, the config file and `--log-dir` directories are created with owner-only permissions (`0600` / `0700`), because telemetry carries `user.email`, `user.id` and organization identifiers
- **Small footprint**: a single static-ish binary (~7 MB stripped), a static musl build for systems without glibc (distroless, Alpine), and a distroless container image
- **Ready-made examples**: Docker Compose, GitHub Actions, and GitLab CI

Design details (request limits, the stdout writer, delivery guarantees): [docs/architecture.md](docs/architecture.md)

## Installation

<!-- standard:install:start -->
### Homebrew (macOS/Linux)

```bash
brew install owayo/otel-logger/otel-logger
```

### Cargo

Requires Rust 1.98 or later.

```bash
cargo install --git https://github.com/owayo/otel-logger --locked
```

### From GitHub Releases

Download the archive for your platform from [Releases](https://github.com/owayo/otel-logger/releases/latest), extract it, and put `otel-logger` on your `PATH`. Each release also includes `SHA256SUMS` for checking the downloads.

| Platform | Archive |
|---|---|
| Linux (x86_64) | `otel-logger-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (x86_64, musl) | `otel-logger-x86_64-unknown-linux-musl.tar.gz` |
| Linux (ARM64) | `otel-logger-aarch64-unknown-linux-gnu.tar.gz` |
| macOS (Intel) | `otel-logger-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `otel-logger-aarch64-apple-darwin.tar.gz` |
| Windows (x86_64) | `otel-logger-x86_64-pc-windows-msvc.zip` |

On macOS, if you downloaded the archive with a browser, remove the quarantine attribute before running it: `xattr -d com.apple.quarantine otel-logger`.

### From Source

Requires [mise](https://mise.jdx.dev/) (the Rust toolchain is pinned in `mise.toml`).

```bash
git clone https://github.com/owayo/otel-logger.git
cd otel-logger
make install
```

`make install` installs to `/usr/local/bin`. Set `INSTALL_PATH` to change it (for example `make install INSTALL_PATH="$HOME/.local/bin"`).
<!-- standard:install:end -->

### Docker

Build the bundled Dockerfile (a distroless image that runs as a non-root user) and start the receiver on the standard OTLP ports:

```bash
docker build -t otel-logger:dev .
docker run --rm -p 4317:4317 -p 4318:4318 otel-logger:dev
```

## Usage

```bash
otel-logger [OPTIONS]
```

```bash
# Listen on the standard OTLP ports and write JSONL to disk
otel-logger --log-file ./otel.jsonl

# Run only the HTTP listener (point gRPC at an unused address)
otel-logger --grpc-addr 127.0.0.1:0 --http-addr 0.0.0.0:4318

# Smoke test from CI
otel-logger --dry-run
```

All options, their environment variables and the `init` subcommand: [docs/cli-reference.md](docs/cli-reference.md)

### Sending telemetry from Claude Code

Claude Code's telemetry contract is environment-variable driven. See [Anthropic monitoring docs](https://code.claude.com/docs/en/monitoring-usage). The simplest way to apply the env vars in every session is the `env` block of `~/.claude/settings.json` (or `.claude/settings.json` per-project):

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

When running under Docker Compose / CI alongside the receiver container, swap `localhost` for the service name (`otel-logger`). The same variables can also be `export`-ed in a shell if you prefer not to use `settings.json`.

### Sending telemetry from OpenAI Codex CLI

Codex contracts on `config.toml` rather than environment variables. See [Codex config reference](https://developers.openai.com/codex/config-reference).

```toml
# $CODEX_HOME/config.toml
[otel]
environment = "local"
log_user_prompt = false # do not transmit prompt content
exporter = { otlp-http = { endpoint = "http://localhost:4318/v1/logs", protocol = "binary", headers = {} } }
metrics_exporter = { otlp-http = { endpoint = "http://localhost:4318/v1/metrics", protocol = "binary", headers = {} } }
trace_exporter = { otlp-http = { endpoint = "http://localhost:4318/v1/traces", protocol = "binary", headers = {} } }
```

When running under Docker Compose / CI alongside the receiver container, swap `localhost` for the service name (`otel-logger`). A working sample lives at [`codex-config/config.toml`](codex-config/config.toml).

Running the receiver as a sidecar in Docker Compose, GitHub Actions and GitLab CI: [docs/integrations.md](docs/integrations.md)

### Usage totals

`GET /stats` always returns the cumulative Claude/Codex usage (tokens, cost, duration) per provider/model/effort as JSON, and `--summary` appends the same totals to stdout whenever a batch changes them. Claude API request logs are preferred over metrics for token/cost usage, and matching metrics are de-duplicated instead of being added twice.

```bash
curl -s http://localhost:4318/stats | jq
```

`/stats` is served on the OTLP/HTTP listener without authentication. The default bind is `0.0.0.0:4318`, so bind to `127.0.0.1` or restrict the port at the firewall if the numbers are sensitive. How each agent's usage is counted and the response format: [docs/usage-stats.md](docs/usage-stats.md)

### Output format

Dynamic payload fields such as service names, span names, metric names, severity text, and attribute keys are escaped before printing.

```text
[trace]  2026-05-07T22:01:14.123Z service=claude-code scope=anthropic.claude_code span=tool.call dur=812ms status=OK trace=4d2... span_id=8ab...
        attrs: {tool.name=Bash, exit_code=0}
[log]    2026-05-07T22:01:14.456Z service=claude-code scope=anthropic.claude_code severity=INFO body="ran command"
[metric] service=claude-code scope=anthropic.claude_code name=claude_code.tokens.input sum=[1234 {model=claude-sonnet-4-6}]
```

In the JSON Lines file, each line is a JSON object with the original protobuf payload preserved (keys are in OTLP/JSON camelCase). Process the file with `jq` or feed it into your warehouse of choice.

```json
{"kind":"traces","resourceSpans":[{"resource":{"attributes":[…]},"scopeSpans":[…]}]}
{"kind":"metrics","resourceMetrics":[…]}
{"kind":"logs","resourceLogs":[…]}
```

## Configuration

`otel-logger` reads `$XDG_CONFIG_HOME/otel-logger/config.toml` on startup (falling back to `~/.config/otel-logger/config.toml`). Use `--config <PATH>` to point at a different file. Every key is optional; missing keys fall back to the built-in default. Precedence (highest wins): CLI flag > environment variable > config file > default.

Generate a fully-commented starter file with `otel-logger init` (`--daemon` writes the preset for long-lived services):

```bash
otel-logger init
```

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

Path expansion, the retention rules of `log-dir` and the daemon preset: [docs/configuration.md](docs/configuration.md)

## Running as a daemon

A long-lived instance must run with `--no-stdout`: the receiver never rotates its stdout stream, and `--log-keep-days` only prunes the JSONL files written by `--log-dir`. One launchd-managed instance grew a 7.6 GB stdout file in 37 days before anyone looked. `otel-logger` warns on stderr at startup when stdout is not a terminal and the human-readable stream is still on.

launchd and systemd units, Docker log drivers, and why rename-based rotators do not work: [docs/daemon.md](docs/daemon.md)

## OTLP proxy forwarding

`otel-logger` can persist received OTLP payloads to JSONL **and** forward them to upstream OTLP collectors at the same time. Claude Code and Codex traffic are split by `service.name` and can target separate endpoints; forwarding requires a JSONL sink (`--log-file` or `--log-dir`), and a slow upstream never blocks the receive path.

```bash
otel-logger \
  --log-file ./otel.jsonl \
  --proxy-anthropic-endpoint https://collector.example.com:4317 \
  --proxy-anthropic-header 'Authorization=env:ANTHROPIC_PROXY_TOKEN'
```

Routing, retries, the TOML form and per-route counters: [docs/proxy.md](docs/proxy.md)

## Development

<!-- standard:dev:start -->
Requires [mise](https://mise.jdx.dev/). Tool versions are pinned in `mise.toml`.

```bash
make setup   # Install the toolchain (mise) and dependencies
make ci      # Run the same checks as CI (no changes)
```

| Command | Description |
|---|---|
| `make setup` | Install the toolchain (mise) and dependencies |
| `make build` | Build a debug binary |
| `make release` | Build a release binary |
| `make run` | Run the debug binary (arguments via ARGS="...") |
| `make test` | Run the tests |
| `make lint` | Run clippy with warnings as errors |
| `make fmt` | Format the code (rewrites files) |
| `make fmt-check` | Check the formatting (no changes) |
| `make check` | Run fmt-check and lint (no changes) |
| `make ci` | Run the same checks as CI (no changes) |
| `make install` | Install the release binary to INSTALL_PATH (default /usr/local/bin) |
| `make uninstall` | Remove the binary from INSTALL_PATH |
| `make clean` | Remove build artifacts |

Run `make` to list every target. Releases are published from GitHub Actions (**Actions → Release → Run workflow**).
<!-- standard:dev:end -->

Replaying saved JSONL through the aggregator, the dependency audit and the Docker targets: [docs/development.md](docs/development.md)

## License

<!-- standard:license:start -->
[MIT](LICENSE)
<!-- standard:license:end -->
