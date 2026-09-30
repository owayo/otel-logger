# Running as a daemon

How to keep `otel-logger` running as a long-lived service without letting its output grow without bound.

## Why `--no-stdout`

When `otel-logger` runs as a long-lived service, always start it with `--no-stdout`. The receiver never rotates or caps its own stdout stream: file descriptor 1 belongs to the parent process (launchd, systemd or the container runtime), and `otel-logger` knows neither the destination path nor its retention contract. `--log-keep-days` retention applies **only** to the daily files `otel-logger` writes into `--log-dir` (JSONL, plus the `--pretty-log` files) — it has no effect on a redirected stdout. Neither launchd's `StandardOutPath` nor a plain shell redirect rotates the target either: one launchd-managed instance quietly grew a 7.6 GB stdout file in 37 days (~200 MB/day) before anyone looked.

Because stdout is never a terminal under launchd, systemd, or Docker, `otel-logger` detects that case at startup and prints a one-time warning to stderr when the human-readable stream is still enabled. The warning suggests `--no-stdout` (or `no-stdout = true` in the config file) for unattended runs, and adding `--pretty-log` to keep the human-readable stream as daily files in `--log-dir`. `--pretty-log` on its own does not silence it, because stdout is still written.

`--no-stdout` suppresses the pretty stream and the `--summary` blocks on stdout, but aggregation itself keeps running — `GET /stats` returns the live totals at any time (see [usage-stats.md](usage-stats.md)). To keep the human-readable stream anyway, write it to files with `--pretty-log` (see [Keeping the human-readable stream](#keeping-the-human-readable-stream)).

To start from a config file that already carries these defaults, use the `--daemon` preset. It only writes the file; it does not register a service or put anything in the background:

```bash
otel-logger init --daemon                             # → ~/.config/otel-logger/config.toml
otel-logger init --daemon -p /etc/otel-logger/config.toml
```

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

The preset leaves `pretty-log = true` commented out because it costs extra disk; see the next section before turning it on.

## Keeping the human-readable stream

`--pretty-log` (`pretty-log = true` in the config file, `OTEL_LOGGER_PRETTY_LOG=1` in the environment) writes the human-readable stream into files that `otel-logger` owns inside `--log-dir`, where rotation and retention are under its control. The files contain the same per-record lines as stdout, plus the `--summary` blocks when `--summary` is on.

It is a separate destination, independent of `--no-stdout`:

| `no-stdout` | `pretty-log` | Human-readable output goes to |
|---|---|---|
| `false` | `false` | stdout |
| `false` | `true` | stdout and the pretty log files |
| `true` | `false` | nowhere (aggregation and `GET /stats` keep running) |
| `true` | `true` | the pretty log files only |

`--pretty-log` alone does not stop a redirected stdout from growing: stdout keeps being written, and the startup warning still fires. For a daemon, use `--no-stdout --pretty-log`.

- **File names**: `<log-dir>/otel-logger.pretty.YYYY-MM-DD.log`, rotated daily by local date — the same boundary as the JSONL files `otel-logger.YYYY-MM-DD`
- **Requires `--log-dir`**: `log-dir` has to be the effective JSONL sink after precedence is resolved. If the effective sink is `log-file`, or there is no JSONL sink at all, startup fails with an error; the pretty files are never written next to a `log-file` or anywhere else implicitly
- **Retention**: `--log-keep-days` prunes the pretty files together with the daily JSONL files, by the same rules ([configuration.md](configuration.md#retention-of-log-dir)). Old pretty files keep being pruned after `--pretty-log` is turned off again, as long as `--log-dir` is used
- **Disk usage**: off by default, because it costs extra disk. In the launchd deployment above, the human-readable stream was about 200 MB/day against about 800 MB/day of JSONL, adding roughly a quarter of the JSONL volume
- **No color**: `--color` applies only to stdout. The pretty files never contain ANSI color codes, even with `color = "always"`; control characters in incoming payloads are escaped exactly as on stdout
- **Best effort**: like stdout, the pretty log is written off the OTLP acknowledgement path through its own bounded queue. Overflow is dropped and reported, and write failures (a full disk, for example) never turn into OTLP errors; JSONL persistence, usage aggregation and proxy forwarding are unaffected. Queue limits and failure reporting: [architecture.md](architecture.md)
- **No `fsync`**: each batch is flushed to the kernel, but the pretty files are not `fsync`'d on shutdown, so a power loss may lose the tail of a pretty file. The JSONL files remain the lossless record and are still `fsync`'d
- **Permissions**: the pretty files carry the same sensitive attributes as the JSONL (such as `user.email`). Like the daily JSONL files, they are created with the process umask by the rotation library, so the directory is what protects them: `otel-logger` creates `--log-dir` as `0700`, and an existing directory keeps its mode
- **One instance per directory**: several `otel-logger` instances sharing one `--log-dir` are not supported

### Migrating an existing launchd agent

If an existing agent still sends the human-readable stream to a file through `StandardOutPath`:

1. Set `no-stdout = true` and `pretty-log = true` in the config file, next to `log-dir` (or add `--no-stdout --pretty-log` to `ProgramArguments`, next to `--log-dir`).
2. Optionally, point `StandardOutPath` at `/dev/null`, as in the example below.
3. Restart the agent: `launchctl kickstart -k gui/$(id -u)/<label>`. `kickstart` restarts the job with the definition launchd has already loaded, so if you edited the plist in steps 1–2, reload it instead with `launchctl bootout` followed by `launchctl bootstrap` (see [macOS (launchd)](#macos-launchd)).
4. Delete the old stdout file by hand. `otel-logger` does not clean up files it did not create.

## macOS (launchd)

Save the agent as `~/Library/LaunchAgents/io.github.owayo.otel-logger.plist` and replace `YOUR_USER` with the account that owns the log directory:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>io.github.owayo.otel-logger</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/otel-logger</string>
    <string>--no-stdout</string>
    <string>--log-dir</string>
    <string>/Users/YOUR_USER/Library/Logs/otel-logger</string>
    <string>--log-keep-days</string>
    <string>10</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>/dev/null</string>
  <key>StandardErrorPath</key>
  <string>/dev/null</string>
</dict>
</plist>
```

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.github.owayo.otel-logger.plist
launchctl print gui/$(id -u)/io.github.owayo.otel-logger          # inspect state
launchctl kickstart -k gui/$(id -u)/io.github.owayo.otel-logger   # restart
launchctl bootout gui/$(id -u)/io.github.owayo.otel-logger        # stop and unload
```

`StandardOutPath` points at `/dev/null` because launchd only redirects the stream — it never rotates the target file. The same is true of `StandardErrorPath`: if you point it at a real file to keep the receiver's own diagnostics, rotating and pruning that file is your responsibility, not launchd's.

To keep the human-readable stream, add `<string>--pretty-log</string>` right after `--no-stdout`: the pretty files then land in the same `--log-dir`, next to the JSONL. For an agent that already writes stdout to a file, see [Migrating an existing launchd agent](#migrating-an-existing-launchd-agent).

## Linux (systemd)

Save the unit as `/etc/systemd/system/otel-logger.service`:

```ini
[Unit]
Description=otel-logger OTLP receiver
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=otel-logger
ExecStart=/usr/local/bin/otel-logger --no-stdout --log-dir /var/log/otel-logger --log-keep-days 10
Restart=on-failure
RestartSec=5s
StandardOutput=null
StandardError=journal

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now otel-logger.service
sudo systemctl status otel-logger.service
journalctl -u otel-logger.service -f
```

`/var/log/otel-logger` has to be writable by the unit's `User=`; the JSONL files inside it are rotated daily and pruned by `--log-keep-days`. Adding `--pretty-log` to `ExecStart=` keeps the human-readable stream in the same directory as daily `otel-logger.pretty.YYYY-MM-DD.log` files, pruned by the same setting.

The two output streams are bounded in different ways, and the difference matters. `StandardError=journal` hands the receiver's diagnostics to journald, which enforces the host's own retention (`SystemMaxUse=`, `MaxRetentionSec=` and friends in `journald.conf`), so that path stays bounded on its own. Appending straight to a file with `StandardOutput=append:/var/log/otel-logger/stdout.log` is what grows without bound — nothing rotates that file.

## Docker log drivers

The default `json-file` logging driver does not rotate, so a container left up permanently accumulates its stdout **and** stderr for the life of the container. `--no-stdout` removes the bulk of that, but the receiver's own diagnostics still go to stderr. If you keep container logs at all, bound them explicitly — either with the `local` driver, which rotates by default, or by giving `json-file` the same options:

```yaml
services:
  otel-logger:
    image: ghcr.io/owayo/otel-logger:latest
    command: ["--no-stdout", "--log-dir", "/var/log/otel-logger", "--log-keep-days", "10"]
    volumes:
      - ./data:/var/log/otel-logger
    logging:
      driver: local        # or json-file, with the same two options
      options:
        max-size: "10m"
        max-file: "3"
```

The bundled [`compose.yaml`](../compose.yaml) deliberately keeps stdout on, so that `docker compose logs otel-logger` stays useful for a short sample run. Add `--no-stdout` and a `logging:` block before leaving that stack up permanently.

## External rotators (newsyslog / logrotate)

`otel-logger` does not re-open its output files on `SIGHUP`, which rules out the usual rename-then-signal rotation: once `logrotate` or `newsyslog` renames the file, the process keeps writing to the old descriptor, the new file stays empty, and the disk space is never reclaimed. `copytruncate` avoids the rename but loses whatever is written between the copy and the truncate, so it must never be pointed at the JSONL sink.

The supported answer is to let `otel-logger` manage its own files — `--log-dir` rotates daily, `--log-keep-days` prunes — and to drop the human-readable stream from stdout with `--no-stdout`, adding `--pretty-log` if you want to keep it as daily files. If a permanent instance really has to keep stdout itself, pipe it into a consumer that rotates on its own (`multilog`, `rotatelogs`, journald) rather than into a rename-based rotator.
