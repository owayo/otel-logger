# Architecture

How `otel-logger` receives, prints, stores and counts telemetry, and the guarantees each step keeps so that exporters neither lose nor duplicate batches.

## How it works

- `tonic` exposes the three OTLP gRPC services (`TraceService`, `MetricsService`, `LogsService`) on port 4317.
- `axum` serves `/v1/traces`, `/v1/metrics`, `/v1/logs` on port 4318 and accepts both `application/x-protobuf` (decoded with `prost`) and `application/json` (decoded via `serde`). The `Content-Type` media type is matched case-insensitively (RFC 9110), so values such as `Application/X-Protobuf; charset=utf-8` are accepted.
- Both transports raise their per-request decode limit to 32 MiB (`OTLP_MAX_REQUEST_BYTES`) so a large batch is never permanently rejected by the 4 MiB / 2 MiB transport defaults.
- `Content-Encoding: gzip` bodies are decompressed before decoding, and the 32 MiB limit is enforced on the decompressed body as the OTLP spec requires.
- Both transports converge on a shared `Sink` that writes pretty stdout, the optional pretty log files (`--pretty-log`) and lossless JSONL.
- `tokio_util::sync::CancellationToken` plus a `tokio::select!` that listens for SIGINT/SIGTERM gives a clean shutdown; an admission gate waits for accepted writes before the final JSONL flush, including writes whose handlers were cancelled.

## Receiving

- Media types are matched case-insensitively with parameters allowed; malformed non-UTF-8 `Content-Type` values return `415 Unsupported Media Type` instead of silently falling back to protobuf
- `Content-Encoding: gzip` requests are accepted (`OTEL_EXPORTER_OTLP_COMPRESSION=gzip`); the size limit applies to the decompressed body
- gRPC/HTTP raise their per-request decode limit to 32 MiB (above `tonic`'s 4 MiB / `axum`'s 2 MiB defaults) so large batches are persisted instead of being permanently rejected with `RESOURCE_EXHAUSTED` / `413` that exporter retries cannot recover from

## Request limits

The 32 MiB limit bounds one request, not the whole process. A gzip body grows to up to 32 MiB as it is decompressed, so a client that opens many connections and never finishes sending could make the receiver hold hundreds of megabytes from a few kilobytes of traffic. The receiver therefore limits how many requests it receives and saves at once, and how long receiving may take:

- HTTP and gRPC share one limit for all signals: 8 requests at a time by default (`--max-inflight-requests`). A request takes its slot before its body is read (for gRPC, before the message is decoded) and keeps it until the batch is saved and counted, even if the client disconnects meanwhile
- A request over the limit waits for a free slot instead of being refused at once. Codex sends logs, traces and metrics from several processes at the same moment, and refusing such a burst would lose batches from exporters that do not retry
- Waiting plus reading the body must finish within 5 seconds of accepting the request (`--receive-timeout-ms`), half of the OTLP exporters' default 10-second timeout. Saving and counting are not cut short; a timeout after saving would make exporters resend a batch that is already stored
- Requests that miss the deadline, or that arrive while 8 times the limit are already waiting, get HTTP `503` / gRPC `Unavailable`, which OTLP exporters retry. `408` is not used because OTLP treats it as non-retryable

## Human-readable stream (stdout and `--pretty-log`)

- The same per-record lines, plus the `--summary` blocks, go to stdout, to the `--pretty-log` files (`<log-dir>/otel-logger.pretty.YYYY-MM-DD.log`), to both, or to neither; the two destinations are independent of each other ([daemon.md](daemon.md#keeping-the-human-readable-stream))
- Color is chosen by severity on stdout only, and turned off automatically when stdout is redirected or `NO_COLOR` is set. The pretty log files never contain ANSI color codes, even with `color = "always"`
- Hardens against terminal escape injection: ANSI escapes and other C0/C1 control characters in incoming payloads, including dynamic labels and attribute keys, are escaped before reaching the terminal or the pretty log files (JSONL output stays lossless)
- Each destination has its own writer task and its own bounded queue (256 batches and 64 MiB of rendered text), off the OTLP acknowledgement path. A slow stdout reader (a paused pager, a stalled log driver) cannot stall responses or the pretty log, and a stalled pretty log cannot stall responses or stdout. Blocking the ACK would make exporters time out and resend batches that were already persisted and counted
- Overflowing output is dropped and reported, with a warning at 1, 2, 4, 8, … dropped batches; JSONL persistence, usage aggregation and proxy forwarding are unaffected. A single batch whose rendered text alone exceeds 64 MiB is dropped and counted the same way, even when the queue is empty: escaping can inflate a payload several times (a NUL byte becomes the 8-character `\u{0000}`)
- If a writer has stopped altogether, the first dropped batch is reported as an error saying so, separately from the "writer is behind" warning, because its output does not come back until the receiver restarts. Shutdown likewise reports a stopped writer separately from one that missed the drain deadline
- Nested attribute values (arrays and key-value lists) are rendered with each string escaped once, so the output grows in proportion to the nesting depth. Escaping the already-rendered inner value again at every level used to double its size per level, letting a few hundred bytes of deeply nested attributes expand to gigabytes
- Write failures (a full disk, for example) never turn into OTLP errors. For the pretty log, and for stdout errors other than a broken pipe, they are reported once when failures start, with the underlying cause (such as `No space left on device`), then as a reminder at most every 10 minutes while writes keep failing, and once more when writes have stayed healthy for a minute
- Writes count as recovered only when a write succeeds at least 60 seconds after the last failure. Failures that recur less than a minute apart stay one outage (one start message, then the 10-minute reminders) even if writes succeed in between, so a nearly full disk that alternates between success and failure does not produce a start/recover pair per batch. After a recovery has been reported, the next failure is reported right away as a new outage
- A broken pipe on stdout (the reader is gone) permanently disables stdout output only; the pretty log keeps being written
- Each batch is flushed to the kernel, but the pretty log files are not `fsync`'d on shutdown, so a power loss may lose the tail of a pretty file; JSONL remains the lossless record
- The pretty log files carry the same sensitive attributes as the JSONL and rely on the same protection: the owner-only `--log-dir` directory (`0700` when `otel-logger` creates it)

## JSON Lines persistence

- JSON Lines are written to one append-only file or to daily-rotated files. Daily files are `fsync`'d through their owned handles before rotation, and the current file on graceful shutdown. Existing archives are not reopened
- Persistence failures surface as HTTP `503 Service Unavailable` / gRPC `Status::unavailable` so OTLP exporters can retry instead of silently dropping payloads. OTLP treats `500` and `Internal` as non-retryable, so those codes would make exporters drop the batch
- Usage totals are updated only after JSONL persistence succeeds, so retried batches are not counted twice
- Each batch, including its newline, is written to the kernel before ACK; failed partial appends are truncated back to the batch start. If rollback fails, subsequent writes are rejected until rollback succeeds
- On open, an incomplete tail after the last newline is removed before new records are appended. Such a batch was never acknowledged. Keep one receiver process per log sink
- Daily JSONL and pretty files use the local calendar date at each write, including after several idle days
- Non-finite doubles (NaN, ±Infinity) are written as the proto3 JSON strings `"NaN"`, `"Infinity"` and `"-Infinity"`; plain `serde_json` would write `null` and lose the value. Output for finite values is byte-for-byte the same as before. `opentelemetry-proto` 0.33 cannot read those strings back yet, so `replay_check` reports such batches as undecodable (upstream: open-telemetry/opentelemetry-rust#3442)
- JSONL write failures report their underlying cause when the outage starts, at most every 10 minutes while it continues, and after 60 seconds without a failure
- JSONL files, the config file and `--log-dir` directories are created with owner-only permissions (`0600` / `0700`); telemetry payloads carry `user.email`, `user.id` and organization identifiers

## Shutdown

- SIGINT and SIGTERM trigger graceful shutdown; new ingestion is rejected before the final synchronization, and admitted writes finish even if their request handlers were cancelled
- The signal handler starts the shutdown before it logs that a signal arrived. Logging used to come first, and when stderr could not be written the handler task died there; because `tokio` replaces the default signal action for the whole process, every later SIGTERM / SIGINT was then ignored and only SIGKILL could stop the receiver
- Connection draining has a 10-second grace period: a client that declares a body and never finishes sending it cannot hold the process hostage and prevent the final `fsync`. A second signal abandons in-flight connections immediately
- JSONL is flushed and `fsync`'d first; the two human-readable writers are then drained concurrently within one shared 5-second deadline, so a stdout stuck in a blocking write (a reader that stopped reading) cannot keep the process from exiting. The drain runs even when the final `fsync` fails, so queued output and the last summary are not discarded
- Diagnostics still queued for stderr are flushed for at most 1 second on exit
- Interrupted and queued proxy batches get a 5-second drain window instead of being discarded outright, so a restart does not silently strand everything the upstream had not yet acknowledged

The 10-second grace bounds connection draining, not the whole shutdown. JSONL disk synchronization is awaited without a timeout; pretty and proxy drains have their own deadlines. A forced kill or power loss is outside the graceful-shutdown guarantee.
