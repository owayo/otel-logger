# OTLP proxy forwarding

`otel-logger` can persist received OTLP payloads to JSONL **and** forward them to one or more upstream OTLP collectors at the same time. Anthropic (Claude Code) and OpenAI (Codex) traffic are split by `service.name` and can target separate endpoints.

- **Requirement**: proxy forwarding requires a JSONL sink (`--log-file` or `--log-dir`) so every accepted payload remains durable even when forwarding fails. Automatic replay to the upstream is planned for Phase B.
- **Routing defaults**: `claude-code` → the Anthropic route, `codex_cli_rs` / `codex_exec` / `codex-app-server` / `codex_mcp_server` → the OpenAI route. Override with a non-empty `service_names` list in the config to add or replace; empty names are rejected at startup so resources without a `service.name` cannot be routed accidentally.
- **Precedence**: CLI transport and headers override a matching built-in route in the config while reusing its endpoint, following CLI > environment > config precedence without requiring the endpoint to be repeated.
- **HTTP endpoint validation**: `http-protobuf` routes require an absolute `http://` or `https://` URL. Query strings and fragments are rejected at startup because OTLP signal paths (`/v1/logs`, `/v1/traces`, and `/v1/metrics`) are appended to the configured endpoint.
- **Failure semantics**: JSONL is persisted first, then the payload is `try_send`'d to the per-route worker. Workers retry with exponential backoff (by default, up to 8 retries after the initial attempt; 200ms → 30s cap). The receive path is never blocked by the upstream, and shutdown cancels both an in-flight request and backoff immediately instead of waiting for the configured request timeout.
- **Shutdown drain**: queued proxy batches get a 5-second drain window instead of being discarded outright, so a restart does not silently strand everything the upstream had not yet acknowledged.
- **Auth**: header values may be written as `env:VAR_NAME` to resolve from an environment variable, so secrets never appear in `ps` output or config files.

## CLI example

```bash
export ANTHROPIC_PROXY_TOKEN=xxxx
export OPENAI_PROXY_TOKEN=yyyy

otel-logger \
  --log-file ./otel.jsonl \
  --proxy-anthropic-endpoint https://collector.example.com:4317 \
  --proxy-anthropic-header 'Authorization=env:ANTHROPIC_PROXY_TOKEN' \
  --proxy-openai-endpoint https://openai-collector.example.com \
  --proxy-openai-transport http-protobuf \
  --proxy-openai-header 'Authorization=env:OPENAI_PROXY_TOKEN'
```

## TOML example

```toml
log-file = "/var/log/otel-logger/otel-logger.jsonl"

[proxy]
queue-capacity = 1024
timeout-ms = 5000
retry-max = 8

[[proxy.routes]]
name = "anthropic"
transport = "grpc"
endpoint = "https://collector.example.com:4317"
[proxy.routes.headers]
Authorization = "env:ANTHROPIC_PROXY_TOKEN"

[[proxy.routes]]
name = "openai"
transport = "http-protobuf"
endpoint = "https://openai-collector.example.com"
[proxy.routes.headers]
Authorization = "env:OPENAI_PROXY_TOKEN"
```

Add more `[[proxy.routes]]` blocks for internal collectors, staging clones, etc. A `service.name` claimed by more than one route is rejected at startup to prevent duplicate delivery.

## Counters

Per-route counters are exposed at `GET /stats`:

```json
{
  "agents": { ... },
  "proxy": {
    "anthropic": { "sent": 42, "failed": 0, "dropped": 0, "queue_depth": 0 },
    "openai":    { "sent": 17, "failed": 1, "dropped": 0, "queue_depth": 0 }
  }
}
```

`queue_depth` is the route's actual bounded-channel occupancy at snapshot time. It is derived directly from Tokio channel capacity, so concurrent receive operations cannot make the counter wrap or report a stale manually maintained value.

## Phase B — crash-safe outbox (planned)

The current implementation (Phase A) keeps every payload in JSONL, but a crash mid-forward can leave in-flight batches un-forwarded. Phase B will track the JSONL byte offset per route and catch up on restart, so that no payload is lost even across restarts. `--proxy-checkpoint-dir` is reserved for that follow-up work.
