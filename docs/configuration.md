# Configuration

Where `otel-logger` reads its config file, how the values are combined with the CLI and the environment, and what the generated starter files contain. The options themselves are listed in [cli-reference.md](cli-reference.md).

## Location

`otel-logger` reads `$XDG_CONFIG_HOME/otel-logger/config.toml` on startup (falling back to `~/.config/otel-logger/config.toml`). Use `--config <PATH>` to point at a different file. Every key is optional; missing keys fall back to the built-in default.

Leading `~` / `~/` is expanded to `$HOME` for `--config`, `otel-logger init --path`, `log-file`, and `log-dir`, including values supplied through environment variables or the config file.

Note: TOML paths are not generally shell-expanded. A leading `~` / `~/` is expanded for the path settings listed above, but embedded environment variables such as `$HOME/logs` are not expanded; write absolute paths for those cases.

## Precedence

**Precedence** (highest wins): CLI flag > environment variable > config file > default. For the mutually exclusive log sinks, this precedence also applies across `log-file` and `log-dir`: specifying `--log-file` ignores a configured `log-dir`, and specifying `--log-dir` ignores a configured `log-file`.

## Retention of `log-dir`

When `log-dir` is used, retention cleanup only removes daily rotated files named `otel-logger.YYYY-MM-DD` whose suffix is a real calendar date. Other files in the same directory, such as `otel-logger.pid`, `otel-logger.stderr.log`, or a standalone `otel-logger.jsonl`, are left untouched. Date-shaped names containing an impossible date and symbolic links are also left untouched.

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
summary = false
color = "auto"  # "auto" | "always" | "never"
# grpc-addr = "0.0.0.0:4317"
# http-addr = "0.0.0.0:4318"
```

## Proxy routes

The `[proxy]` table and the `[[proxy.routes]]` blocks configure OTLP proxy forwarding. See [proxy.md](proxy.md) for the keys and an example.
