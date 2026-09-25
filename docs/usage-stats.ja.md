# 累計トークン統計

`otel-logger` が Claude Code と Codex のテレメトリから token・コスト・所要時間の累計をどう作るかと、その値を読む 2 つの方法 (`GET /stats` と `--summary`) をまとめます。

## 集計元

`otel-logger` は両エージェントのトークン / コスト / duration を集計し、2 つの方法で公開します。Claude は metrics で token/cost の初期値を集計し、対応する API request log が届いた場合はログ側へ置き換えたうえで request count/duration も補完します。Claude の API request ログと metrics の両方に token/cost が存在する場合は、request 単位で即時に届き metrics より新しい分まで含みうるログ側を優先し、対応する metrics は二重に加算しません。

Codex の token usage は Codex が出す 2 つの形を重複排除して集計します。現在のローカルログと CI artifact では `codex.sse_event` / `response.completed` log が最も完全な token counter を持つため、これが最初の token source ならそれを採用し、`codex.turn.token_usage` metrics は最初または唯一の token source として観測された場合の fallback として使います。

| エージェント | tokens & cost                                              | request_count                       | duration                              | メタデータ                                                       |
|--------------|------------------------------------------------------------|-------------------------------------|---------------------------------------|------------------------------------------------------------------|
| claude-code  | metrics `claude_code.token.usage` + `claude_code.cost.usage` | log `claude_code.api_request`         | log `claude_code.api_request.duration_ms` | —                                                                |
| codex        | model ごとの最初の source: log `codex.sse_event` / `response.completed` または metric `codex.turn.token_usage` (Histogram、`total` は無視) | metric `codex.conversation.turn.count` | metric `codex.turn.e2e_duration_ms`     | log/span event `codex.conversation_starts` (`provider`/`effort` 補完) |

## Claude Code

Anthropic のログは `model` のサフィックス (`[1m]` 等) を落とすため、メトリクス側で観測したフル名 (`claude-opus-4-7[1m]`) を canonical 表として保持し、後続のログ側 bare 名を同じ bucket にマージします。`aggregationTemporality=DELTA` のみ受け入れ、Cumulative は警告ログ付きで破棄します。

## Codex

`service.name` で TUI (`codex_cli_rs`) / Exec (`codex_exec`) / Apps Server (`codex-app-server`、Codex 0.140.0+) / MCP Server (`codex_mcp_server`、Codex 0.146.1 / 0.147.0 の実ログで確認) を Codex として認識します。Apps Server は `codex.turn.*` などの metrics を送らず logs / traces だけを送ってくるため、ここを取りこぼすと Apps Server 経由の token usage が累計から欠落します。

Codex は同じ usage を SSE 完了ログと turn token metrics の両方で送るため、`otel-logger` は model ごとに最初に観測した token source (SSE `response.completed` ログか `codex.turn.token_usage` metric) を採用し、その model ではもう一方の token counter を二重計上防止のため無視します。ローカル実ログでは SSE 先着と metric 先着の両方があり、metric 先着時も後着 SSE と token 種別ごとの合計が完全一致することを確認しています。usage を持たない WebSocket `response.completed` は、別 usage として加算しません。

`tool_token_count` は他の token 種別と重複するため加算しません。SSE ログの `cache_write_token_count` と metric の token type `cache_write_input` は、どちらも `cache_creation_tokens` に集計します。実ログでは `input_token_count == tool_token_count` かつ output/cache-read/cache-write/reasoning がすべて 0 の tool-only `response.completed` が turn metrics / `handle_responses` span usage から除外されているため、`otel-logger` でも token usage としては数えません。ただし、同じ形でも cache write が 1 以上なら実 usage として集計します。この扱いは実際の CI ログ (Codex 0.150.1、2 conversation / SSE 完了 27 件) で検証済みで、SSE 合計から tool-only 分を差し引いた値が `codex.turn.token_usage` metric と全 token 種別で完全一致しました (input 1,513,467 / output 17,618 / cached_input 1,316,096 / reasoning_output 9,550 / cache_write_input 0)。そのため、SSE 先着でも metric 先着でも累計は同じ値になります。

2026-09-06 の Codex 0.153.4 ローカル実ログでは、先行する `gpt-5.6-sol/xhigh` conversation の turn metric snapshot が届いた後にも、同じ model/effort の別 conversation から新しい SSE 完了ログが続く順序を確認しました。Logs を採用した model はその選択を維持し、metric snapshot の重複分だけを除外しながら、後続 conversation の SSE usage を加算し続けます。

`session_task.turn` / `session_task.review` span の `codex.turn.token_usage.*` も同じ usage の別表現なので、trace span から token は計上しません。

tracing exporter によっては span event の name が source location になり、論理名 `codex.conversation_starts` は `event.name` 属性へ格納されます。provider/effort の補完では、この現行形式と論理名を直接持つ旧形式の両方を受け付けます。log record では論理名が body、`event.name` 属性、top-level の `LogRecord.event_name` のいずれに入る形式も受け付けます。

Codex 0.144.1+ の SSE completion に含まれる `model_reasoning_effort` を直接採用し、session 到着前でも実測した `gpt-5.6-terra` の high/xhigh を保持します。Codex token log が `conversation_starts` より先に届いた場合は、一時的な `effort=unknown` bucket を、後から届いた provider/model/effort bucket へ統合します。pending usage は provider/model/effort/conversation 単位で保留し、遅れて `codex.conversation_starts` が届いた時は、当該 conversation だけを確定 provider (Azure や OpenAI-compatible endpoint) へ移します。既知の SSE effort は保持し、SSE に effort が無い場合だけ session の値で補完します。

`conversation.id` が付与された SSE 完了ログは、対応する `codex.conversation_starts` メタデータにだけ紐付け、別 conversation の直近 session には決してフォールバックしません。メタデータがまだ届いていない場合は一旦 `effort=unknown` バケットに格納し、後から到着した時点で正しい effort バケットへ統合します。これにより、複数 conversation が混在したり、日をまたいだ継続セッションでも token usage が誤った effort バケットに移動しません。`handle_responses` span から effort を再取得する際も `conversation.id` を尊重し、別 conversation の session を壊しません。

`conversation.id` / effort を持たない Codex turn metrics は、同じ `service.name` で最後に観測した session だけを fallback に使います。TUI (`codex_cli_rs`) と Exec (`codex_exec`) が同時稼働しても、片方の provider / effort がもう片方の metrics に混入しません。

## カウンタの保護

- provider/model/effort を固定 allowlist で制限せず動的に保持するため、`gpt-5.6-terra` や Fable のような新しい識別子も lossless に記録する。component 内に `/` や `%` が含まれても別バケットと衝突しない
- token / duration / cost の属性値や metric 値に NaN / Infinity / 範囲外の巨大な double が混入しても、parse 時点で弾いて累計を破壊しない (信頼できない telemetry source からの `i64::MAX` / `u64::MAX` 飽和値や `cost_usd=inf` の混入を防ぐ)
- 累計 counter は saturating arithmetic で加算し、極端な batch が繰り返されても token 合計の wrap や `cost_usd=inf` を起こさない
- 累計使用量は JSONL 永続化に成功してから更新するため、retry された batch を二重計上しない

## `GET /stats` (常時有効)

> **Note**
> `/stats` は OTLP/HTTP と同じ listener で提供され、認証はありません。累計トークン数・USD コスト・provider/model/effort の内訳・proxy 転送の件数がそのまま取得できます。既定の bind は `0.0.0.0:4318` なので、これらの数値を秘匿したい場合は `127.0.0.1` に bind するか、ファイアウォールで信頼できないネットワークから遮断してください。

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
        "OpenAI/gpt-5.5/xhigh":    { "provider": "OpenAI", "model": "gpt-5.5",     "effort": "xhigh", ... },
        "OpenAI/gpt-5.4-mini/low": { "provider": "OpenAI", "model": "gpt-5.4-mini", "effort": "low",   ... }
      }
    }
  }
}
```

bucket key は `provider/model/effort` 形式です。各 component 内の `/` と `%` は、それぞれ `%2F` / `%25` に percent-encode されるため、任意の telemetry label が別 bucket と衝突しません。Codex 側は ChatGPT / OpenAI が cost を出さないため `cost_usd` は常に `0` です。フラグ無しで常に取得できます。OTLP proxy 転送を有効にしていれば、route ごとの送信件数も `proxy` に入ります ([proxy.ja.md](proxy.ja.md))。

## `--summary` (stdout、opt-in)

`--summary` (または `OTEL_LOGGER_SUMMARY=1` / 設定ファイルの `summary = true`) を有効にすると、使用量の累計が更新されるバッチを受信するたびに `[stats:<agent>]` ブロックが追記されます。

```text
[stats:claude-code] requests=81 input=65509 output=85207 cache_read=8924351 cache_create=724182 reasoning=0 cost=$10.8716 duration=1262.400s since=2026-05-08T...
        breakdown provider=anthropic model=claude-opus-4-7[1m] effort=max: requests=74 input=1253 output=82097 cache_read=8924351 cache_create=671142 reasoning=0 cost=$10.7155 duration=1234.560s
        breakdown provider=anthropic model=claude-haiku-4-5-20251001 effort=unknown: requests=7 input=64256 output=3110 cache_read=0 cache_create=53040 reasoning=0 cost=$0.1561 duration=27.840s
```

カウンタはプロセス生存中の累計です。otel-logger を再起動するとリセットされます。
