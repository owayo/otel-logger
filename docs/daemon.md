# Running as a daemon

How to keep `otel-logger` running as a long-lived service without letting its output grow without bound.

## Why `--no-stdout`

When `otel-logger` runs as a long-lived service, always start it with `--no-stdout`. The receiver never rotates or caps its own stdout stream, and `--log-keep-days` retention applies **only** to the JSONL files written by `--log-dir` — it has no effect on a redirected stdout. Neither launchd's `StandardOutPath` nor a plain shell redirect rotates the target either: one launchd-managed instance quietly grew a 7.6 GB stdout file in 37 days (~200 MB/day) before anyone looked.

Because stdout is never a terminal under launchd, systemd, or Docker, `otel-logger` detects that case at startup and prints a one-time warning to stderr when the human-readable stream is still enabled, pointing at `--no-stdout` (or `no-stdout = true` in the config file).

`--no-stdout` suppresses the pretty stream and the `--summary` blocks, but aggregation itself keeps running — query `GET /stats` for the live totals instead of reading a log file (see [usage-stats.md](usage-stats.md)).

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
summary = false
color = "auto"  # "auto" | "always" | "never"
# grpc-addr = "0.0.0.0:4317"
# http-addr = "0.0.0.0:4318"
```

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

`/var/log/otel-logger` has to be writable by the unit's `User=`; the JSONL files inside it are rotated daily and pruned by `--log-keep-days`.

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

The supported answer is to let `otel-logger` manage its own files — `--log-dir` rotates daily, `--log-keep-days` prunes — and to drop the human-readable stream with `--no-stdout`. If a permanent instance really has to keep stdout, pipe it into a consumer that rotates on its own (`multilog`, `rotatelogs`, journald) rather than into a rename-based rotator.
