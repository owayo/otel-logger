<h1 align="center">otel-logger</h1>

<p align="center">
  <strong>Claude Code / Codex の OTLP テレメトリを stdout と JSON Lines で記録する受信サーバ</strong>
</p>

<p align="center">
  <a href="https://github.com/owayo/otel-logger/actions/workflows/ci.yml">
    <img alt="CI" src="https://github.com/owayo/otel-logger/actions/workflows/ci.yml/badge.svg?branch=main">
  </a>
  <a href="https://github.com/owayo/otel-logger/releases/latest">
    <img alt="Version" src="https://img.shields.io/github/v/release/owayo/otel-logger">
  </a>
  <a href="LICENSE">
    <img alt="License" src="https://img.shields.io/github/license/owayo/otel-logger">
  </a>
</p>

<p align="center">
  <a href="README.md">English</a> | 日本語
</p>

---

## 概要

`otel-logger` は CI コンテナ上で動く AI コーディングエージェント (**Claude Code** / **OpenAI Codex CLI**) の隣に置く、Rust 製の小さな OTLP 受信サーバです。
OTLP/gRPC を `:4317`、OTLP/HTTP を `:4318` で受け、Traces / Metrics / Logs をデコードして 2 つの経路に出力します。

- **stdout**: 1 件ずつ整形した可読ログ (CI ログでそのまま読める)
- **JSON Lines** (`--log-file`): 元の OTLP 構造を欠落させない永続化先

Jaeger や Honeycomb への転送は行いません。CI 実行中にエージェントが吐くテレメトリを、開発者がいつも見ている場所 (CI ログ / アーティファクト) に出すことだけを目的にしています。

## 特徴

- OTLP/gRPC (4317) と OTLP/HTTP (4318) を同一プロセスで起動
- HTTP は `application/x-protobuf` と `application/json` の両方を受信
- severity 別の色付きで stdout 表示 (リダイレクト時や `NO_COLOR` で自動 OFF)
  - 受信した payload に ANSI escape や C0/C1 制御文字が含まれていても terminal にそのまま出さず escape する (terminal escape injection 対策、JSONL は lossless のまま)
- JSON Lines は graceful shutdown 時に `fsync`
  - JSONL の永続化に失敗した場合は HTTP 5xx / gRPC `Status::internal` を返し、OTLP exporter 側に retry させる (受信 payload を黙って捨てない)
- Claude/Codex の累計使用量を `/stats` と `--summary` で表示し、logs と metrics の二重計上を回避
  - Codex で `effort=unknown` に積まれた pending token は `conversation.id` 単位で保留し、対応する `codex.conversation_starts` が後から届いた conversation の分だけを新 effort バケットへ振り替える (並行する別 conversation の token を巻き込まない)
- SIGINT / SIGTERM 対応 (`docker stop` で末尾バッチが落ちない)
- Stripped で約 7 MB の単一バイナリ。distroless コンテナイメージ同梱
- Docker Compose / GitHub Actions / GitLab CI の利用例を同梱

## 動作環境

- **OS**: Linux、macOS
- **Rust**: 1.88 以上 (ソースからビルドする場合のみ。`tonic 0.14` の MSRV に合わせており、edition 2024 を使用)。同梱の Dockerfile は余裕を持たせて `rust:1.90` を使用

## インストール

### Homebrew (macOS / Linux)

```bash
brew install owayo/otel-logger/otel-logger
```

tap には `arm64_sonoma` / `sonoma` / `x86_64_linux` 向けのプリビルド bottle が同梱されています。他のプラットフォームは `cargo install` でソースビルドにフォールバック (`depends_on "rust"` で Rust 1.88+ を取得)。

### ソースからビルド

```bash
cargo install --path .
```

### バイナリをダウンロード

[Releases](https://github.com/owayo/otel-logger/releases) からプラットフォームに合うバイナリを取得し、`$PATH` に置きます。提供アーティファクト:
`otel-logger-{aarch64,x86_64}-apple-darwin.tar.gz`、
`otel-logger-{x86_64,aarch64}-unknown-linux-gnu.tar.gz`、
`otel-logger-x86_64-unknown-linux-musl.tar.gz` (distroless / Alpine 向けの static build)、
`otel-logger-x86_64-pc-windows-msvc.zip`。

### Docker

```bash
docker build -t otel-logger:dev .
docker run --rm -p 4317:4317 -p 4318:4318 otel-logger:dev
```

## 使い方

```bash
otel-logger [OPTIONS]
```

### オプション

| オプション      | 短縮形 | 既定値           | 環境変数                   | 説明                                                       |
|----------------|-------|------------------|---------------------------|------------------------------------------------------------|
| `--config`     |       | (自動)           | `OTEL_LOGGER_CONFIG`      | TOML 設定ファイルのパス (下記参照)                          |
| `--grpc-addr`  |       | `0.0.0.0:4317`   | `OTEL_LOGGER_GRPC_ADDR`   | gRPC バインドアドレス                                       |
| `--http-addr`  |       | `0.0.0.0:4318`   | `OTEL_LOGGER_HTTP_ADDR`   | HTTP バインドアドレス (protobuf / JSON 両対応)             |
| `--log-file`   |       | (なし)           | `OTEL_LOGGER_LOG_FILE`    | 受信内容を JSON Lines で追記出力 (`--log-dir` と排他)        |
| `--log-dir`    |       | (なし)           | `OTEL_LOGGER_LOG_DIR`     | 指定ディレクトリに日次ローテーションで JSONL を出力 (`otel-logger.YYYY-MM-DD`、ローカルタイム) |
| `--log-keep-days` |    | `10`             | `OTEL_LOGGER_LOG_KEEP_DAYS` | `--log-dir` 利用時に保持する日数                          |
| `--no-stdout`  |       | `false`          | `OTEL_LOGGER_NO_STDOUT`   | 整形 stdout の出力を抑止                                    |
| `--summary`    |       | `false`          | `OTEL_LOGGER_SUMMARY`     | 使用量の累計が更新された時に累計サマリーを stdout に追記       |
| `--color`      |       | `auto`           | `OTEL_LOGGER_COLOR`       | `auto` / `always` / `never` (`NO_COLOR` を尊重)            |
| `--dry-run`    | `-n`  | `false`          |                           | 両 listener の同時 bind を含む起動チェックを実施して終了     |
| `--help`       | `-h`  |                  |                           | ヘルプ表示                                                 |
| `--version`    | `-V`  |                  |                           | バージョン表示                                             |

### 設定ファイル (`~/.config/otel-logger/config.toml`)

起動時に `$XDG_CONFIG_HOME/otel-logger/config.toml` (未設定なら
`~/.config/otel-logger/config.toml`) を自動で読みます。`--config <PATH>` で
別ファイルを指定可能。すべてのキーは任意で、無いキーはデフォルト値を使います。

**優先順位** (上が強い): CLI フラグ > 環境変数 > 設定ファイル > 既定値。
相互排他のログ出力先にもこの優先順位が適用されます。`--log-file` を指定した場合は
設定ファイル側の `log-dir` を無視し、`--log-dir` を指定した場合は設定ファイル側の
`log-file` を無視します。

`log-dir` 利用時の保持期間 cleanup は、`otel-logger.YYYY-MM-DD` 形式の日次ローテーション
ファイルだけを削除対象にします。同じディレクトリにある `otel-logger.pid`、
`otel-logger.stderr.log`、単体の `otel-logger.jsonl` などは削除しません。

コメント入りのテンプレートは `init` コマンドで生成できます:

```bash
otel-logger init                    # → ~/.config/otel-logger/config.toml
otel-logger init -p /etc/foo.toml   # → 任意のパス
otel-logger init -f                 # 既存ファイルを上書き
```

生成されるファイルの中身:

```toml
# ~/.config/otel-logger/config.toml
log-file = "/var/log/otel-logger/otel-logger.jsonl"
# 代わりに日次ローテーションを使う場合 (`log-file` と排他):
# log-dir = "/var/log/otel-logger"
# log-keep-days = 10                 # 既定: 10
no-stdout = false
summary = false
color = "auto"  # "auto" | "always" | "never"
# grpc-addr = "0.0.0.0:4317"
# http-addr = "0.0.0.0:4318"
```

注意: TOML 内のパスはシェル展開されません。絶対パスを書くか、シェル展開が必要なら
`OTEL_LOGGER_LOG_FILE` 環境変数を使ってください。

サーバ自身の診断ログは **stderr** に出ます。`OTEL_LOGGER_LOG=debug` のように `tracing-subscriber` の env filter 文法でフィルタ可能です。

### 使用例

```bash
# 標準ポートで起動し、JSONL もディスクに残す
otel-logger --log-file ./otel.jsonl

# HTTP のみ動かしたい (gRPC は使わないアドレスへ)
otel-logger --grpc-addr 127.0.0.1:0 --http-addr 0.0.0.0:4318

# CI でのスモークテスト
otel-logger --dry-run
```

### 使用量サマリー

`--summary` を指定すると、Claude/Codex の使用量サンプルを取り込むたびに累計サマリーを stdout へ追記します。`GET /stats` は同じ累計値を常に JSON で返します。Claude の API request ログと metrics の両方に token/cost が存在する場合は、request 単位で即時に届き metrics より新しい分まで含みうるログ側を優先し、対応する metrics は二重に加算しません。

## Claude Code からの送信

Claude Code は環境変数で OTLP exporter を有効化します。
詳細は [Anthropic monitoring ドキュメント](https://code.claude.com/docs/en/monitoring-usage) を参照。
毎回 export せずに済ませたい場合は `~/.claude/settings.json` (またはプロジェクト直下の `.claude/settings.json`) の `env` ブロックに書くのが手軽です。

```jsonc
{
  "env": {
    "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
    "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA": "1",
    "OTEL_LOGS_EXPORTER": "otlp",
    "OTEL_METRICS_EXPORTER": "otlp",
    "OTEL_TRACES_EXPORTER": "otlp",
    "OTEL_EXPORTER_OTLP_PROTOCOL": "http/protobuf",
    "OTEL_EXPORTER_OTLP_ENDPOINT": "http://localhost:4318",
    "OTEL_EXPORTER_OTLP_TIMEOUT": "5000",
    "OTEL_RESOURCE_ATTRIBUTES": "service.name=claude-code,deployment.environment=local"
  }
}
```

Docker Compose / CI で同じネットワークの受信コンテナへ送る場合は、`localhost` をサービス名 (`otel-logger`) に置き換えてください。`settings.json` を使わずシェルで `export` してもかまいません。

## OpenAI Codex CLI からの送信

Codex は環境変数ではなく `config.toml` の `[otel]` セクションが公式契約です。
詳細は [Codex config reference](https://developers.openai.com/codex/config-reference) を参照。

```toml
# $CODEX_HOME/config.toml
[otel]
environment = "local"
log_user_prompt = false # プロンプトの内容は送信しない
exporter = { otlp-http = { endpoint = "http://localhost:4318/v1/logs", protocol = "binary", headers = {} } }
metrics_exporter = { otlp-http = { endpoint = "http://localhost:4318/v1/metrics", protocol = "binary", headers = {} } }
trace_exporter = { otlp-http = { endpoint = "http://localhost:4318/v1/traces", protocol = "binary", headers = {} } }
```

Docker Compose / CI で同じネットワークの受信コンテナへ送る場合は、`localhost` をサービス名 (`otel-logger`) に置き換えてください。

そのまま使えるサンプルを [`codex-config/config.toml`](codex-config/config.toml) に同梱しています。

## Docker Compose

`otel-logger` をサイドカーとして、Claude Code / Codex を別コンテナで動かす [`compose.yaml`](compose.yaml) を同梱しています。

```bash
docker compose up otel-logger
docker compose run --rm claude-code-sample
docker compose run --rm codex-sample
```

サーバ側のログは `docker compose logs otel-logger`、JSONL は `./data/otel-logger.jsonl` に出力されます。

## CI での利用例

### GitHub Actions

```yaml
jobs:
  ai-job:
    runs-on: ubuntu-latest
    services:
      otel-logger:
        image: ghcr.io/owayo/otel-logger:latest
        ports:
          - 4317:4317
          - 4318:4318
        options: >-
          --health-cmd "/usr/local/bin/otel-logger --dry-run"
          --health-interval 10s
          --health-timeout 3s
          --health-retries 3
    env:
      CLAUDE_CODE_ENABLE_TELEMETRY: "1"
      OTEL_LOGS_EXPORTER: otlp
      OTEL_METRICS_EXPORTER: otlp
      OTEL_TRACES_EXPORTER: otlp
      CLAUDE_CODE_ENHANCED_TELEMETRY_BETA: "1"
      OTEL_EXPORTER_OTLP_PROTOCOL: http/protobuf
      OTEL_EXPORTER_OTLP_ENDPOINT: http://localhost:4318
      OTEL_RESOURCE_ATTRIBUTES: "service.name=claude-code,deployment.environment=gha"
    steps:
      - uses: actions/checkout@v4
      - run: npm install -g @anthropic-ai/claude-code
      - run: claude --print "your prompt here"
```

### GitLab CI

```yaml
ai-job:
  image: node:20-bookworm-slim
  services:
    - name: ghcr.io/owayo/otel-logger:latest
      alias: otel-logger
  variables:
    CLAUDE_CODE_ENABLE_TELEMETRY: "1"
    OTEL_LOGS_EXPORTER: otlp
    OTEL_METRICS_EXPORTER: otlp
    OTEL_TRACES_EXPORTER: otlp
    CLAUDE_CODE_ENHANCED_TELEMETRY_BETA: "1"
    OTEL_EXPORTER_OTLP_PROTOCOL: http/protobuf
    OTEL_EXPORTER_OTLP_ENDPOINT: http://otel-logger:4318
    OTEL_RESOURCE_ATTRIBUTES: "service.name=claude-code,deployment.environment=gitlab"
  script:
    - npm install -g @anthropic-ai/claude-code
    - claude --print "your prompt here"
```

## 累計トークン統計

`otel-logger` は両エージェントのトークン / コスト / duration を集計し、2 つの方法で公開します。
Claude は token/cost を metrics 主軸、request count/duration を log 補完で扱います。
Codex の token usage は Codex が出す 2 つの形を重複排除して集計します。実際の Codex 0.129/0.130 のローカルログと CI artifact では `codex.sse_event` / `response.completed` log が最も完全な token counter を持つため、これが最初の token source ならそれを採用し、`codex.turn.token_usage` metrics は最初または唯一の token source として観測された場合の fallback として使います。

| エージェント | tokens & cost                                              | request_count                       | duration                              | メタデータ                                                       |
|--------------|------------------------------------------------------------|-------------------------------------|---------------------------------------|------------------------------------------------------------------|
| claude-code  | metrics `claude_code.token.usage` + `claude_code.cost.usage` | log `claude_code.api_request`         | log `claude_code.api_request.duration_ms` | —                                                                |
| codex        | log `codex.sse_event` / `response.completed` または fallback metric `codex.turn.token_usage` (Histogram、`total` は無視) | metric `codex.conversation.turn.count` | metric `codex.turn.e2e_duration_ms`     | log/span event `codex.conversation_starts` (`provider`/`effort` 補完) |

Anthropic のログは `model` のサフィックス (`[1m]` 等) を落とすため、メトリクス側で観測したフル名 (`claude-opus-4-7[1m]`) を canonical 表として保持し、後続のログ側 bare 名を同じ bucket にマージします。`aggregationTemporality=DELTA` のみ受け入れ、Cumulative は警告ログ付きで破棄します。

Codex は同じ usage を SSE 完了ログと turn token metrics の両方で送るため、`otel-logger` は model ごとに最初に観測した token source を採用し、その model ではもう一方の token counter を二重計上防止のため無視します。`tool_token_count` は他の token 種別と重複するため加算しません。実ログでは `input_token_count == tool_token_count` かつ output/cache/reasoning がすべて 0 の tool-only `response.completed` が turn metrics / `handle_responses` span usage から除外されているため、`otel-logger` でも token usage としては数えません。Codex token log が `conversation_starts` より先に届いた場合は、一時的な `effort=unknown` bucket を、後から届いた provider/model/effort bucket へ統合します。`conversation.id` が付与された SSE 完了ログは、対応する `codex.conversation_starts` メタデータにだけ紐付け、別 conversation の直近 session には決してフォールバックしません。メタデータがまだ届いていない場合は一旦 `effort=unknown` バケットに格納し、後から到着した時点で正しい effort バケットへ統合します。これにより、複数 conversation が混在したり、日をまたいだ継続セッションでも token usage が誤った effort バケットに移動しません。

### `GET /stats` (常時有効)

```bash
$ curl -s http://localhost:4318/stats | jq
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

bucket key は `provider/model/effort` 形式です。Codex 側は ChatGPT / OpenAI が cost を出さないため `cost_usd` は常に `0` です。フラグ無しで常に取得できます。

### `--summary` (stdout、opt-in)

`--summary` (または `OTEL_LOGGER_SUMMARY=1` / 設定ファイルの `summary = true`) を有効にすると、使用量の累計が更新されるバッチを受信するたびに `[stats:<agent>]` ブロックが追記されます。

```
[stats:claude-code] requests=81 input=65509 output=85207 cache_read=8924351 cache_create=724182 reasoning=0 cost=$10.8716 duration=21.04m since=2026-05-08T...
        breakdown provider=anthropic model=claude-opus-4-7[1m] effort=max: requests=74 input=1253 output=82097 cache_read=8924351 cache_create=671142 reasoning=0 cost=$10.7155
        breakdown provider=anthropic model=claude-haiku-4-5-20251001 effort=unknown: requests=7 input=64256 output=3110 cache_read=0 cache_create=53040 reasoning=0 cost=$0.1561
```

カウンタはプロセス生存中の累計です。otel-logger を再起動するとリセットされます。

## 出力フォーマット

### stdout (pretty)

```
[trace]  2026-05-07T22:01:14.123Z service=claude-code scope=anthropic.claude_code span=tool.call dur=812ms status=OK trace=4d2... span_id=8ab...
        attrs: {tool.name=Bash, exit_code=0}
[log]    2026-05-07T22:01:14.456Z service=claude-code scope=anthropic.claude_code severity=INFO body="ran command"
[metric] service=claude-code scope=anthropic.claude_code name=claude_code.tokens.input sum=[1234 {model=claude-sonnet-4-6}]
```

### JSON Lines (`--log-file`)

1 行 1 JSON で OTLP の元構造をそのまま保持しています (キーは OTLP/JSON の camelCase です)。

```json
{"kind":"traces","resourceSpans":[{"resource":{"attributes":[…]},"scopeSpans":[…]}]}
{"kind":"metrics","resourceMetrics":[…]}
{"kind":"logs","resourceLogs":[…]}
```

`jq` や任意のデータウェアハウスに流し込んで分析できます。

## 開発

```bash
make build      # cargo build
make test       # cargo test
make check      # cargo check --all-targets
make clippy     # cargo clippy --all-targets -- -D warnings
make fmt        # cargo fmt
make run        # --log-file ./otel-logger.jsonl 付きで起動
make docker     # コンテナイメージのビルド
```

## 内部構造

- `tonic` が OTLP の 3 つの gRPC サービス (`TraceService` / `MetricsService` / `LogsService`) をポート 4317 で公開
- `axum` がポート 4318 で `/v1/traces`、`/v1/metrics`、`/v1/logs` を受け、`application/x-protobuf` (prost デコード) と `application/json` (serde デコード) の両方に対応
- 両トランスポートが共通の `Sink` に流れ込み、stdout pretty と JSONL の両方へ書き出す
- `tokio_util::sync::CancellationToken` と SIGINT / SIGTERM を待つ `tokio::select!` で graceful shutdown。gRPC / HTTP task の終了を待ってから最後に JSONL を flush するため、末尾のバッチも欠落しません

## ライセンス

[MIT](LICENSE)
