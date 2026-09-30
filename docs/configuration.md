# Configuration

Where `otel-logger` reads its config file, how the values are combined with the CLI and the environment, and what the generated starter files contain. The options themselves are listed in [cli-reference.md](cli-reference.md).

## Location

`otel-logger` reads `$XDG_CONFIG_HOME/otel-logger/config.toml` on startup (falling back to `~/.config/otel-logger/config.toml`). Use `--config <PATH>` to point at a different file. Every key is optional; missing keys fall back to the built-in default.

Leading `~` / `~/` is expanded to `$HOME` for `--config`, `otel-logger init --path`, `log-file`, and `log-dir`, including values supplied through environment variables or the config file.

Note: TOML paths are not generally shell-expanded. A leading `~` / `~/` is expanded for the path settings listed above, but embedded environment variables such as `$HOME/logs` are not expanded; write absolute paths for those cases.

## Precedence

**Precedence** (highest wins): CLI flag > environment variable > config file > default. For the mutually exclusive log sinks, this precedence also applies across `log-file` and `log-dir`: specifying `--log-file` ignores a configured `log-dir`, and specifying `--log-dir` ignores a configured `log-file`.

`pretty-log` needs `log-dir` to be the sink that wins this resolution. A config file with `log-dir` and `pretty-log = true` fails to start when `--log-file` is given on the command line, and so does `pretty-log` with no JSONL sink at all. `otel-logger` never writes the pretty files next to a `log-file` or anywhere else implicitly.

## Retention of `log-dir`

When `log-dir` is used, `log-keep-days` applies to two kinds of daily files:

- the JSONL files `otel-logger.YYYY-MM-DD`
- the `pretty-log` files `otel-logger.pretty.YYYY-MM-DD.log`

Files older than `log-keep-days` days, judged by their modification time, are removed at startup and again whenever the date changes while the receiver is running (`0` is clamped to a 1-day minimum). Old pretty files keep being pruned after `pretty-log` is turned off again, as long as `log-dir` stays in use.

Cleanup only removes those two kinds of names, and only when the date in the name is a real calendar date. Other files in the same directory, such as `otel-logger.pid`, `otel-logger.stderr.log`, or a standalone `otel-logger.jsonl`, are left untouched. Date-shaped names containing an impossible date, symbolic links, and pretty-shaped names without the exact `.log` suffix (such as `otel-logger.pretty.2026-09-30` or `otel-logger.pretty.2026-09-30.log.gz`) are also left untouched.

## Starter files

Generate a fully-commented starter file with the bundled command:

```bash
otel-logger init                    # → ~/.config/otel-logger/config.toml
otel-logger init -p /etc/foo.toml   # → custom path
otel-logger init -f                 # overwrite an existing file
otel-logger init --daemon           # preset for long-lived services
```

The generated file looks like:

```toml
# ~/.config/otel-logger/config.toml
log-file = "/var/log/otel-logger/otel-logger.jsonl"
# Or, daily-rotated output (mutually exclusive with `log-file`):
# log-dir = "/var/log/otel-logger"
# log-keep-days = 10                 # default: 10
no-stdout = false
# pretty-log = true                  # requires `log-dir`: also write the human-readable stream to daily files
summary = false
color = "auto"  # "auto" | "always" | "never"
# grpc-addr = "0.0.0.0:4317"
# http-addr = "0.0.0.0:4318"
```

`--daemon` writes a different starter file, tuned for long-lived instances: `no-stdout = true` plus daily-rotated JSONL with retention. See [daemon.md](daemon.md).

```toml
# ~/.config/otel-logger/config.toml
log-dir = "/var/log/otel-logger"
log-keep-days = 10                 # default: 10
# Or, a single append-only file (mutually exclusive with `log-dir`, never rotated):
# log-file = "/var/log/otel-logger/otel-logger.jsonl"
no-stdout = true
# pretty-log = true                # also write the human-readable stream to daily files (extra disk)
summary = false
color = "auto"  # "auto" | "always" | "never"
# grpc-addr = "0.0.0.0:4317"
# http-addr = "0.0.0.0:4318"
```

Both files carry `pretty-log = true` commented out. When enabled, it writes the human-readable stream to daily files in `log-dir` as well, independently of `no-stdout`; [daemon.md](daemon.md#keeping-the-human-readable-stream) lists how the two settings combine.

## Proxy routes

The `[proxy]` table and the `[[proxy.routes]]` blocks configure OTLP proxy forwarding. See [proxy.md](proxy.md) for the keys and an example.
