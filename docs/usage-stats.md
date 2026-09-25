# Usage statistics

How `otel-logger` turns Claude Code and Codex telemetry into cumulative token, cost and duration totals, and the two ways to read them: `GET /stats` and `--summary`.

## Sources

`otel-logger` aggregates token / cost / duration usage from both agents and exposes the running totals two ways. Claude metrics provide the initial token/cost totals, while a matching API request log supersedes them and also provides request count/duration. Claude API request logs are treated as the preferred source for token/cost usage when both logs and metrics are present, because logs arrive per request and can include usage that has not yet been exported as metrics. Matching metrics are de-duplicated instead of being added twice.

Codex token usage is deduplicated across the two shapes Codex emits: current local and CI logs provide the most complete token counters on `codex.sse_event` / `response.completed`, and `codex.turn.token_usage` metrics remain the fallback when they are the first or only token source observed.

| Agent       | tokens & cost                                            | request_count                       | duration                           | metadata                                                         |
|-------------|----------------------------------------------------------|-------------------------------------|------------------------------------|------------------------------------------------------------------|
| claude-code | metrics `claude_code.token.usage` + `claude_code.cost.usage` | log `claude_code.api_request`         | log `claude_code.api_request.duration_ms` | —                                                                |
| codex       | first source per model: log `codex.sse_event` / `response.completed` or metric `codex.turn.token_usage` (Histogram, `total` ignored) | metric `codex.conversation.turn.count` | metric `codex.turn.e2e_duration_ms` | log/span event `codex.conversation_starts` for `provider`/`effort` |

## Claude Code

Anthropic logs strip variant suffixes from `model` (e.g. `claude-opus-4-7`) while metrics carry the full name (`claude-opus-4-7[1m]`). The aggregator canonicalizes log-side bare names to whichever full name was last seen on a metric, so 1M and standard variants do not fragment into separate buckets. Only `aggregationTemporality=DELTA` is honored; cumulative points are dropped with a warning.

## Codex

Supported Codex processes are recognised via `service.name`: TUI (`codex_cli_rs`), Exec (`codex_exec`), Apps Server (`codex-app-server`, Codex 0.140.0+), and MCP Server (`codex_mcp_server`, observed in Codex 0.146.1/0.147.0). Apps-Server-only deployments — which emit logs/traces but no `codex.turn.*` metrics — are still aggregated.

Codex emits both SSE completion logs and turn token metrics for the same usage. `otel-logger` accepts the first token source observed for each model (the first SSE `response.completed` log or `codex.turn.token_usage` metric) and ignores the other source for that model's token counters to avoid double-counting. Real local logs contain both arrival orders; when metrics arrived first, their per-class totals exactly matched the later SSE values. WebSocket `response.completed` events without usage are not counted as separate usage.

`tool_token_count` is not added because it overlaps the other token classes. `cache_write_token_count` in SSE logs and the metric token type `cache_write_input` both contribute to `cache_creation_tokens`. Real logs also include tool-only `response.completed` events where `input_token_count == tool_token_count` and output/cache-read/cache-write/reasoning are all zero; Codex excludes those from turn metrics and `handle_responses` span usage, so `otel-logger` does not count them as token usage. An otherwise tool-only-shaped completion with positive cache-write usage remains counted. This was verified against a real CI log (Codex 0.150.1, two conversations, 27 SSE completions): the SSE totals minus the tool-only events matched `codex.turn.token_usage` exactly for every token class (input 1,513,467 / output 17,618 / cached_input 1,316,096 / reasoning_output 9,550 / cache_write_input 0), so either arrival order produces the same cumulative numbers.

The 2026-09-06 local capture from Codex 0.153.4 also showed a second `gpt-5.6-sol/xhigh` conversation continuing to emit SSE completions after the first conversation's turn-metric snapshot. Once Logs is selected for that model, it remains selected: the snapshot is skipped as a duplicate, while the later conversation's SSE usage continues to be counted.

`session_task.turn` / `session_task.review` spans can also carry `codex.turn.token_usage.*`; those are mirrors of the same usage, so trace spans are not used as token sources.

Depending on the tracing exporter, a span event may use its source location as the event name and carry the logical `codex.conversation_starts` name in the `event.name` attribute. Both that current shape and the legacy direct event name are accepted for provider/effort metadata. For log records, the logical name may be in the body, the `event.name` attribute, or the top-level `LogRecord.event_name`; all three forms are accepted.

Codex 0.144.1+ `model_reasoning_effort` on SSE completions is used directly, preserving observed `gpt-5.6-terra` high/xhigh usage even before its session arrives. If Codex token logs arrive before `conversation_starts`, the temporary `effort=unknown` bucket is folded into the later provider/model/effort bucket when the session metadata arrives. Pending usage is tracked by provider/model/effort/conversation, so a delayed `codex.conversation_starts` moves only that conversation to the confirmed provider (Azure or another OpenAI-compatible endpoint), retaining a known SSE effort and using the session effort only when SSE omitted it.

When `conversation.id` is present on an SSE completion log, the aggregator uses only the matching `codex.conversation_starts` metadata — it never falls back to the last observed session of a different `conversation.id`. If the matching `conversation_starts` has not been seen yet, the entry lands in an `effort=unknown` bucket and is merged into the correct effort bucket once the metadata catches up. This keeps interleaved or long-running Codex conversations from moving token usage into the wrong effort bucket. `handle_responses` spans also respect `conversation.id` when re-deriving `effort`, so a span for one conversation never overwrites another conversation's session.

Codex turn metrics do not carry `conversation.id` or reasoning effort. Their last-session fallback is therefore isolated by `service.name`, preventing a concurrent TUI (`codex_cli_rs`) and Exec (`codex_exec`) process from assigning one process's provider/effort metadata to the other's metrics.

## Counter safety

- Provider/model/effort values are kept dynamically instead of using a model allowlist, so newly introduced identifiers such as `gpt-5.6-terra` and Fable are retained losslessly; `/` and `%` inside components remain collision-free
- Non-finite or out-of-range numeric attributes and metric values (`NaN`, `±Infinity`, huge `double`s) on tokens, durations and cost are rejected at parse time so untrusted telemetry sources cannot poison cumulative counters with saturated `i64::MAX` / `u64::MAX` or `cost_usd=inf`
- Cumulative counters use saturating arithmetic, so repeated extreme batches cannot wrap token totals or turn cost into `Infinity`
- Usage totals are updated only after JSONL persistence succeeds, so retried batches are not counted twice

## `GET /stats` (always on)

> **Note**
> `/stats` is served from the same listener as OTLP/HTTP and requires no authentication. It exposes cumulative token counts, USD cost, the provider/model/effort breakdown and proxy forwarding counters. Since the default bind is `0.0.0.0:4318`, keep the port off untrusted networks (bind to `127.0.0.1`, or restrict it at the firewall) if those numbers are sensitive.

```bash
curl -s http://localhost:4318/stats | jq
```

```json
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

Bucket keys are formatted as `provider/model/effort`; `/` and `%` inside each component are percent-encoded as `%2F` and `%25` so arbitrary telemetry labels cannot collide. Codex's `cost_usd` is always `0` because the OpenAI/ChatGPT side does not emit cost. This endpoint is always available — no flag required. With OTLP proxy forwarding enabled, the response also carries per-route counters under `proxy` ([proxy.md](proxy.md)).

## `--summary` (stdout, opt-in)

When `--summary` (or `OTEL_LOGGER_SUMMARY=1` / `summary = true` in the config) is enabled, otel-logger appends a `[stats:<agent>]` block right after every batch that changes cumulative usage totals:

```text
[stats:claude-code] requests=81 input=65509 output=85207 cache_read=8924351 cache_create=724182 reasoning=0 cost=$10.8716 duration=1262.400s since=2026-05-08T...
        breakdown provider=anthropic model=claude-opus-4-7[1m] effort=max: requests=74 input=1253 output=82097 cache_read=8924351 cache_create=671142 reasoning=0 cost=$10.7155 duration=1234.560s
        breakdown provider=anthropic model=claude-haiku-4-5-20251001 effort=unknown: requests=7 input=64256 output=3110 cache_read=0 cache_create=53040 reasoning=0 cost=$0.1561 duration=27.840s
```

Counters are process-lifetime cumulative; restarting otel-logger resets them.
