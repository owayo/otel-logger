# OTLP proxy 転送

受信した OTLP payload を **JSONL に保存しつつ** 上流の OTLP collector にも転送する proxy モードがあります。Claude Code (Anthropic 系) と Codex (OpenAI 系) の 2 系統を `service.name` で振り分け、それぞれ別の endpoint に送れます。

- **前提**: proxy を有効化するときは `--log-file` か `--log-dir` のどちらかを必ず指定する (転送に失敗しても受理済み payload を JSONL に残すため。上流への自動再送は Phase B で実装予定)
- **振り分け**: 組み込み既定で `claude-code` → Anthropic route、`codex_cli_rs` / `codex_exec` / `codex-app-server` / `codex_mcp_server` / `Codex Desktop` / `codex-exec-server` → OpenAI route。config で空でない `service-names` を明示すれば上書き可能。空の名前は startup 時に reject し、`service.name` が無い resource の誤転送を防ぐ
- **優先順位**: 組み込み route と同名の config endpoint はそのまま利用しつつ、CLI の transport / header で上書きできる。endpoint を CLI で重ねて指定しなくても CLI > 環境変数 > config の優先順位を守る
- **HTTP endpoint の検証**: `http-protobuf` route には絶対 `http://` / `https://` URL を指定する。設定値の末尾へ signal 別パス (`/v1/logs`、`/v1/traces`、`/v1/metrics`) を追加するため、query と fragment は startup 時に reject する
- **失敗時挙動**: JSONL 保存が成功してから proxy に `try_send` する fire-and-forget。route worker が OTLP 仕様で再送できる失敗だけを指数バックオフで retry する (既定では初回送信後に最大 8 回、200ms → 30s cap)。受信 endpoint は proxy の遅延に影響されない。shutdown 時は backoff 中だけでなく送信中の request も即座に中断し、設定した request timeout を待たない
- **終了時の送り切り**: 送信中・backoff 中に中断した batch を先頭に戻し、queue に残った batch と共に 5 秒間の drain で送り切る。再起動のたびに「上流がまだ受け取っていない分」を無言で失わないため
- **認証**: header 値に `env:VAR_NAME` を書くと環境変数から解決する。secret をプロセス一覧や config ファイルに平文で残さないためこちらを推奨。未設定や Unicode として読めない値は起動時に拒否し、診断には変数名と原因だけを載せて値を露出させない

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

## プロファイルを指定したクライアントの metrics だけを転送する

共有 receiver では、route ごとに resource 属性の完全一致条件を指定できます。元の resource がすべての条件を満たした場合だけ転送します。必要なキーの欠落・重複や、文字列以外の値は不一致として扱います。同じ値の重複も受け付けません。この条件は logs・traces・metrics のすべてに適用されます。

```toml
log-file = "./otel.jsonl"

[[proxy.routes]]
name = "work-metrics"
service-names = ["claude-code"]
signal-types = ["metrics"]
transport = "http-protobuf"
endpoint = "https://collector.example.com"
metric-attribute-removals = ["user.account_uuid", "user.account_id"]

[proxy.routes.resource-attributes]
"analytics.profile" = "work"

[proxy.routes.metric-attribute-overrides]
"user.email" = "you@example.com"
```

転送するクライアントの `OTEL_RESOURCE_ATTRIBUTES` に `analytics.profile=work` を設定します。この route は、それ以外のプロファイルを転送しません。異なる resource 条件を設定しても、同じ `service.name` を複数の route に割り当てられません。

| route の設定 | 型 | 動作 |
|---|---|---|
| `resource-attributes` | 文字列から文字列へのテーブル | resource 上ですべての条件を要求する。空なら追加の条件なし |
| `metric-attribute-overrides` | 文字列から文字列へのテーブル | 属性を追加・置換する。同じキーが複数あれば1個の文字列属性に置き換える |
| `metric-attribute-removals` | 文字列の配列 | metric point から該当キーの属性をすべて削除する |

属性の書き換えは条件で絞った後の転送用コピーに対して行います。対象は gauge・sum・histogram・exponential histogram・summary の全5種類です。resource 属性、log、span、exemplar は変更しません。JSONL、人が読める出力、累計使用量には元の payload を使います。

置換・削除を指定するときは、`service.name` 以外の空でないキーを resource 条件に含める必要があります。同じキーの置換と削除は併用できません。不正な組み合わせは起動時に拒否します。組み込み route の endpoint や transport を CLI で上書きしても、設定ファイルの resource 条件と属性の書き換え設定は維持します。再送方法と `/stats` の送信件数は従来どおりです。

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

## 再送できる失敗

HTTP は `429` / `502` / `503` / `504` だけを再送します。`3xx`、`400`、`401`、`413`、`500` などは再送しません。gRPC は `Cancelled` / `DeadlineExceeded` / `Aborted` / `OutOfRange` / `Unavailable` / `DataLoss`、および有効な `google.rpc.RetryInfo` 付きの `ResourceExhausted` を再送します。上流から応答が届く前の失敗 (接続の拒否や切断、timeout) は、HTTP と gRPC のどちらでも再送します。gRPC クライアントは、応答の前に切れた接続を `Unknown` / `Internal` として報告することがあり、この場合も再送します。上流が返した `Unknown` / `Internal` は再送しません。HTTP の `Retry-After` (秒数または HTTP-date) と gRPC RetryInfo が指定する待機時間が backoff より長い場合は、上流の指定を尊重します。詳細は [OTLP の失敗処理仕様](https://opentelemetry.io/docs/specs/otlp/#failures)を参照してください。

shutdown で中断した batch は drain の結果で一度だけ件数へ反映します。期限を超えた分は `dropped`、恒久的な送信失敗は `failed` です。payload は JSONL に残りますが、Phase A では自動再生しません。
