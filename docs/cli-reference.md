# CLI reference

Every option of `otel-logger` and of its `init` subcommand. The config file keys are described in [configuration.md](configuration.md).

## Synopsis

```bash
otel-logger [OPTIONS]
otel-logger init [--path <PATH>] [--force] [--daemon]
```

Without a subcommand, `otel-logger` runs the OTLP receiver.

## Options

| Option         | Short | Default          | Env                       | Description                                              |
|----------------|-------|------------------|---------------------------|----------------------------------------------------------|
| `--config`     |       | (auto)           | `OTEL_LOGGER_CONFIG`      | Path to a TOML config file (see [configuration.md](configuration.md)) |
| `--grpc-addr`  |       | `0.0.0.0:4317`   | `OTEL_LOGGER_GRPC_ADDR`   | gRPC bind address (OTLP/gRPC)                            |
| `--http-addr`  |       | `0.0.0.0:4318`   | `OTEL_LOGGER_HTTP_ADDR`   | HTTP bind address (OTLP/HTTP, both protobuf and JSON)    |
| `--log-file`   |       | (none)           | `OTEL_LOGGER_LOG_FILE`    | Append received telemetry as JSON Lines (mutually exclusive with `--log-dir`) |
| `--log-dir`    |       | (none)           | `OTEL_LOGGER_LOG_DIR`     | Write daily-rotated JSONL into this directory: `otel-logger.YYYY-MM-DD` (local time) |
| `--log-keep-days` |    | `10`             | `OTEL_LOGGER_LOG_KEEP_DAYS` | Days of rotated JSONL to keep when `--log-dir` is used (`0` is clamped to a 1-day minimum) |
| `--no-stdout`  |       | `false`          | `OTEL_LOGGER_NO_STDOUT`   | Suppress the human-readable stdout stream (required for daemons — see [daemon.md](daemon.md)) |
| `--summary`    |       | `false`          | `OTEL_LOGGER_SUMMARY`     | Append cumulative usage summary when usage totals change |
| `--color`      |       | `auto`           | `OTEL_LOGGER_COLOR`       | `auto` / `always` / `never` (honors `NO_COLOR`)          |
| `--dry-run`    | `-n`  | `false`          |                           | Validate startup, including simultaneous listener bind, then exit |
| `--proxy-anthropic-endpoint` | | (none) | `OTEL_LOGGER_PROXY_ANTHROPIC_ENDPOINT` | Forward `service.name=claude-code` payloads to this upstream OTLP endpoint (see [proxy.md](proxy.md)) |
| `--proxy-anthropic-transport` | | `grpc` | `OTEL_LOGGER_PROXY_ANTHROPIC_TRANSPORT` | `grpc` or `http-protobuf`                                 |
| `--proxy-anthropic-header` | | (none) | `OTEL_LOGGER_PROXY_ANTHROPIC_HEADERS` | `Key=Value` header (`env:VAR_NAME` resolves from env); repeatable |
| `--proxy-openai-endpoint` | | (none) | `OTEL_LOGGER_PROXY_OPENAI_ENDPOINT` | Forward Codex (`codex_cli_rs` / `codex_exec` / `codex-app-server` / `codex_mcp_server`) payloads |
| `--proxy-openai-transport` | | `grpc` | `OTEL_LOGGER_PROXY_OPENAI_TRANSPORT` | Same as above                                             |
| `--proxy-openai-header` | | (none) | `OTEL_LOGGER_PROXY_OPENAI_HEADERS` | Same as above                                             |
| `--proxy-checkpoint-dir` | | (next to JSONL) | `OTEL_LOGGER_PROXY_CHECKPOINT_DIR` | Directory for forward checkpoints (reserved for Phase B)  |
| `--help`       | `-h`  |                  |                           | Show help                                                |
| `--version`    | `-V`  |                  |                           | Show version                                             |

The boolean flags (`--no-stdout`, `--summary`) accept the usual truthy spellings through their environment variables — `1` / `0`, `true` / `false`, `yes` / `no`, `on` / `off` — so `OTEL_LOGGER_NO_STDOUT=1` in a systemd unit or a compose file works. An explicit `false` also wins over `true` in the config file, matching the documented precedence. Empty `OTEL_LOGGER_*` variables are treated as unset rather than as an empty value, so leaving a placeholder in `environment:` does not prevent startup.

Values coming from `OTEL_LOGGER_PROXY_*` are never printed by `--help`; they can hold credentials.

## `init`

`otel-logger init` writes a fully-commented starter config file. It refuses to overwrite an existing file unless `--force` is given.

| Option | Short | Description |
|---|---|---|
| `--path` | `-p` | Destination path (default: the path the receiver reads, `$XDG_CONFIG_HOME/otel-logger/config.toml` or `~/.config/otel-logger/config.toml`) |
| `--force` | `-f` | Overwrite an existing file |
| `--daemon` | | Write the preset for long-lived services (`no-stdout = true` and daily-rotated JSONL). It only writes the file; it does not register a service or put anything in the background |

```bash
otel-logger init                    # → ~/.config/otel-logger/config.toml
otel-logger init -p /etc/foo.toml   # → custom path
otel-logger init -f                 # overwrite an existing file
otel-logger init --daemon           # preset for long-lived services
```

## Diagnostics

Internal logs (the receiver's own diagnostics) go to **stderr** and respect `OTEL_LOGGER_LOG=debug` (`tracing-subscriber` env filter syntax).
