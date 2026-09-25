# OTLP proxy 転送

受信した OTLP payload を **JSONL に保存しつつ** 上流の OTLP collector にも転送する proxy モードがあります。Claude Code (Anthropic 系) と Codex (OpenAI 系) の 2 系統を `service.name` で振り分け、それぞれ別の endpoint に送れます。

- **前提**: proxy を有効化するときは `--log-file` か `--log-dir` のどちらかを必ず指定する (転送に失敗しても受理済み payload を JSONL に残すため。上流への自動再送は Phase B で実装予定)
- **振り分け**: 組み込み既定で `claude-code` → Anthropic route、`codex_cli_rs` / `codex_exec` / `codex-app-server` / `codex_mcp_server` → OpenAI route。config で空でない `service_names` を明示すれば上書き可能。空の名前は startup 時に reject し、`service.name` が無い resource の誤転送を防ぐ
- **優先順位**: 組み込み route と同名の config endpoint はそのまま利用しつつ、CLI の transport / header で上書きできる。endpoint を CLI で重ねて指定しなくても CLI > 環境変数 > config の優先順位を守る
- **HTTP endpoint の検証**: `http-protobuf` route には絶対 `http://` / `https://` URL を指定する。設定値の末尾へ signal 別パス (`/v1/logs`、`/v1/traces`、`/v1/metrics`) を追加するため、query と fragment は startup 時に reject する
- **失敗時挙動**: JSONL 保存が成功してから proxy に `try_send` する fire-and-forget。route worker が指数バックオフで retry する (既定では初回送信後に最大 8 回、200ms → 30s cap)。受信 endpoint は proxy の遅延に影響されない。shutdown 時は backoff 中だけでなく送信中の request も即座に中断し、設定した request timeout を待たない
- **終了時の送り切り**: proxy の queue に残った batch は破棄せず 5 秒間の drain で送り切る。再起動のたびに「上流がまだ受け取っていない分」を無言で失わないため
- **認証**: header 値に `env:VAR_NAME` を書くと環境変数から解決する。secret をプロセス一覧や config ファイルに平文で残さないためこちらを推奨

## CLI での指定例

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

## 設定ファイルの例

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

`[[proxy.routes]]` は追加可能なので、社内 collector や staging 転送などを増やせます。ただし異なる route が同じ `service.name` を主張するのは startup 時に error として reject します (二重送信 / 順序不定を避けるため)。

## 送信件数

route ごとの送信累計は `GET /stats` の `proxy` フィールドで観測できます:

```json
{
  "agents": { ... },
  "proxy": {
    "anthropic": { "sent": 42, "failed": 0, "dropped": 0, "queue_depth": 0 },
    "openai":    { "sent": 17, "failed": 1, "dropped": 0, "queue_depth": 0 }
  }
}
```

`queue_depth` は snapshot 時点における route の bounded channel の実占有数です。Tokio channel の capacity から直接算出するため、送受信が並行しても手動カウンタの underflow や古い値は返しません。

## Phase B (今後の予定) — crash-safe outbox

現行 (Phase A) は「JSONL には確実に残るが、process crash 時に in-flight batch が転送されない可能性がある」段階です。Phase B では JSONL の byte-offset を per-route checkpoint として保持し、起動時に catch-up 走査して欠測ゼロを厳密に担保する予定です。このため `--proxy-checkpoint-dir` フラグ・checkpoint ディレクトリの配置場所は先行して用意されています。
