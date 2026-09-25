# Architecture

How `otel-logger` receives, prints, stores and counts telemetry, and the guarantees each step keeps so that exporters neither lose nor duplicate batches.

## How it works

- `tonic` exposes the three OTLP gRPC services (`TraceService`, `MetricsService`, `LogsService`) on port 4317.
- `axum` serves `/v1/traces`, `/v1/metrics`, `/v1/logs` on port 4318 and accepts both `application/x-protobuf` (decoded with `prost`) and `application/json` (decoded via `serde`). The `Content-Type` media type is matched case-insensitively (RFC 9110), so values such as `Application/X-Protobuf; charset=utf-8` are accepted.
- Both transports raise their per-request decode limit to 32 MiB (`OTLP_MAX_REQUEST_BYTES`) so a large batch is never permanently rejected by the 4 MiB / 2 MiB transport defaults.
- `Content-Encoding: gzip` bodies are decompressed before decoding, and the 32 MiB limit is enforced on the decompressed body as the OTLP spec requires.
- Both transports converge on a shared `Sink` that writes pretty stdout and lossless JSONL.
- `tokio_util::sync::CancellationToken` plus a `tokio::select!` that listens for SIGINT/SIGTERM gives a clean shutdown; gRPC/HTTP tasks are awaited before the final JSONL flush so the trailing batch never disappears.

## Receiving

- Media types are matched case-insensitively with parameters allowed; malformed non-UTF-8 `Content-Type` values return `415 Unsupported Media Type` instead of silently falling back to protobuf
- `Content-Encoding: gzip` requests are accepted (`OTEL_EXPORTER_OTLP_COMPRESSION=gzip`); the size limit applies to the decompressed body
- gRPC/HTTP raise their per-request decode limit to 32 MiB (above `tonic`'s 4 MiB / `axum`'s 2 MiB defaults) so large batches are persisted instead of being permanently rejected with `RESOURCE_EXHAUSTED` / `413` that exporter retries cannot recover from

## stdout stream

- Color is chosen by severity and turned off automatically when stdout is redirected or `NO_COLOR` is set
- Hardens against terminal escape injection: ANSI escapes and other C0/C1 control characters in incoming payloads, including dynamic labels and attribute keys, are escaped before reaching the terminal (JSONL output stays lossless)
- Written by a dedicated writer task off the OTLP acknowledgement path, so a slow stdout reader (a paused pager, a stalled log driver) cannot stall responses. Blocking the ACK would make exporters time out and resend batches that were already persisted and counted. Overflowing output is dropped and reported; JSONL persistence, usage aggregation and proxy forwarding are unaffected

## JSON Lines persistence

- JSON Lines are written to one append-only file or to daily-rotated files, and `fsync`'d on graceful shutdown
- Persistence failures surface as HTTP `503 Service Unavailable` / gRPC `Status::unavailable` so OTLP exporters can retry instead of silently dropping payloads. OTLP treats `500` and `Internal` as non-retryable, so those codes would make exporters drop the batch
- Usage totals are updated only after JSONL persistence succeeds, so retried batches are not counted twice
- Each batch is `flush`'d to the kernel before ACK so an unexpected crash never leaves the last write trapped in `BufWriter`'s in-memory buffer
- JSONL files, the config file and `--log-dir` directories are created with owner-only permissions (`0600` / `0700`); telemetry payloads carry `user.email`, `user.id` and organization identifiers

## Shutdown

- SIGINT and SIGTERM trigger a graceful shutdown, so no batch is lost under `docker stop`
- The shutdown is bounded by a 10-second grace period: a client that declares a body and never finishes sending it cannot hold the process hostage and prevent the final `fsync`. A second signal abandons in-flight connections immediately
- Queued proxy batches get a 5-second drain window instead of being discarded outright, so a restart does not silently strand everything the upstream had not yet acknowledged
