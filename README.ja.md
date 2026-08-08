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
- **JSON Lines** (`--log-file` / `--log-dir`): 元の OTLP 構造を欠落させない永続化先。単一の追記ファイルまたは日次ローテーションファイルとして保存

Jaeger や Honeycomb への転送は行いません。CI 実行中にエージェントが吐くテレメトリを、開発者がいつも見ている場所 (CI ログ / アーティファクト) に出すことだけを目的にしています。

## 特徴

- OTLP/gRPC (4317) と OTLP/HTTP (4318) を同一プロセスで起動
- HTTP は `application/x-protobuf` と `application/json` の両方を受信
  - media type は大文字小文字を区別せず parameter 付きも受理する。非 UTF-8 の不正な `Content-Type` は protobuf へ暗黙フォールバックせず `415 Unsupported Media Type` を返す
- severity 別の色付きで stdout 表示 (リダイレクト時や `NO_COLOR` で自動 OFF)
  - 受信した payload の動的ラベルや属性キーに ANSI escape / C0/C1 制御文字が含まれていても terminal にそのまま出さず escape する (terminal escape injection 対策、JSONL は lossless のまま)
- JSON Lines は単一の追記ファイルまたは日次ローテーションファイルへ保存し、graceful shutdown 時に `fsync`
  - JSONL の永続化に失敗した場合は HTTP 5xx / gRPC `Status::internal` を返し、OTLP exporter 側に retry させる (受信 payload を黙って捨てない)
  - 累計使用量は JSONL 永続化に成功してから更新するため、retry された batch を二重計上しない
  - 各 batch は ACK 前に `BufWriter::flush` で kernel まで書き出すため、process crash で末尾の write がメモリバッファに取り残されることがない
  - gRPC / HTTP の 1 リクエスト上限を 32 MiB (`tonic` 既定 4 MiB / `axum` 既定 2 MiB より引き上げ) にし、大きな batch を `RESOURCE_EXHAUSTED` / `413` で恒久拒否せず保存する (exporter の retry でも回復できない欠落を防ぐ)
- Claude/Codex の累計使用量を `/stats` と `--summary` で表示し、logs と metrics の二重計上を回避
  - `service.name` で TUI (`codex_cli_rs`) / Exec (`codex_exec`) / Apps Server (`codex-app-server`、Codex 0.140.0+) すべてを Codex として認識。Apps Server は `codex.turn.*` などの metrics を送らず logs / traces だけを送ってくるため、ここを取りこぼすと Apps Server 経由の token usage が累計から欠落する
  - provider/model/effort を固定 allowlist で制限せず動的に保持するため、`gpt-5.6-terra` や Fable のような新しい識別子も lossless に記録する。component 内に `/` や `%` が含まれても別バケットと衝突しない
  - Codex は model ごとに最初に観測した SSE `response.completed` ログまたは `codex.turn.token_usage` metric を token source として採用する。usage を持たない WebSocket `response.completed` や、`session_task.turn` / `session_task.review` などの trace span 上に出る同一 usage の別表現は、別 usage として加算しない
  - Codex 0.144.1+ の SSE completion に含まれる `model_reasoning_effort` を直接採用し、session 到着前でも実測した `gpt-5.6-terra` の high/xhigh を保持する。pending usage は provider/model/effort/conversation 単位で保留し、遅れて `codex.conversation_starts` が届いた時は、当該 conversation だけを確定 provider (Azure や OpenAI-compatible endpoint) へ移す。既知の SSE effort は保持し、SSE に effort が無い場合だけ session の値で補完する
  - `handle_responses` span から effort を再取得する際も `conversation.id` を尊重し、別 conversation の session を壊さない
  - token / duration / cost の属性値や metric 値に NaN / Infinity / 範囲外の巨大な double が混入しても、parse 時点で弾いて累計を破壊しない (信頼できない telemetry source からの `i64::MAX` / `u64::MAX` 飽和値や `cost_usd=inf` の混入を防ぐ)
  - 累計 counter は saturating arithmetic で加算し、極端な batch が繰り返されても token 合計の wrap や `cost_usd=inf` を起こさない
- SIGINT / SIGTERM 対応 (`docker stop` で末尾バッチが落ちない)
- Stripped で約 7 MB の単一バイナリ。distroless コンテナイメージ同梱
- Docker Compose / GitHub Actions / GitLab CI の利用例を同梱

## 動作環境

- **OS**: Linux、macOS、Windows
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
| `--log-keep-days` |    | `10`             | `OTEL_LOGGER_LOG_KEEP_DAYS` | `--log-dir` 利用時に保持する日数 (`0` を渡しても最低 1 日は残す) |
| `--no-stdout`  |       | `false`          | `OTEL_LOGGER_NO_STDOUT`   | 整形 stdout の出力を抑止                                    |
| `--summary`    |       | `false`          | `OTEL_LOGGER_SUMMARY`     | 使用量の累計が更新された時に累計サマリーを stdout に追記       |
| `--color`      |       | `auto`           | `OTEL_LOGGER_COLOR`       | `auto` / `always` / `never` (`NO_COLOR` を尊重)            |
| `--dry-run`    | `-n`  | `false`          |                           | 両 listener の同時 bind を含む起動チェックを実施して終了     |
| `--proxy-anthropic-endpoint` | | (なし) | `OTEL_LOGGER_PROXY_ANTHROPIC_ENDPOINT` | `service.name=claude-code` の受信 payload を転送する上流 OTLP endpoint (詳細は [OTLP proxy 転送](#otlp-proxy-転送) 節) |
| `--proxy-anthropic-transport` | | `grpc` | `OTEL_LOGGER_PROXY_ANTHROPIC_TRANSPORT` | `grpc` / `http-protobuf`                                    |
| `--proxy-anthropic-header` | | (なし) | `OTEL_LOGGER_PROXY_ANTHROPIC_HEADERS` | `Key=Value` 形式 (`env:VAR_NAME` で環境変数解決)。複数指定可 |
| `--proxy-openai-endpoint` | | (なし) | `OTEL_LOGGER_PROXY_OPENAI_ENDPOINT` | Codex 系 (`codex_cli_rs` / `codex_exec` / `codex-app-server`) の転送先 |
| `--proxy-openai-transport` | | `grpc` | `OTEL_LOGGER_PROXY_OPENAI_TRANSPORT` | 同上                                                        |
| `--proxy-openai-header` | | (なし) | `OTEL_LOGGER_PROXY_OPENAI_HEADERS` | 同上                                                        |
| `--proxy-checkpoint-dir` | | (JSONL の隣) | `OTEL_LOGGER_PROXY_CHECKPOINT_DIR` | 転送 checkpoint 用ディレクトリ (Phase B 用に予約)              |
| `--help`       | `-h`  |                  |                           | ヘルプ表示                                                 |
| `--version`    | `-V`  |                  |                           | バージョン表示                                             |

### 設定ファイル (`~/.config/otel-logger/config.toml`)

起動時に `$XDG_CONFIG_HOME/otel-logger/config.toml` (未設定なら
`~/.config/otel-logger/config.toml`) を自動で読みます。`--config <PATH>` で
別ファイルを指定可能。すべてのキーは任意で、無いキーはデフォルト値を使います。

`--config`、`otel-logger init --path`、`log-file`、`log-dir` では、先頭の
`~` / `~/` を `$HOME` に展開します。環境変数や設定ファイル経由の値も同じ扱いです。

**優先順位** (上が強い): CLI フラグ > 環境変数 > 設定ファイル > 既定値。
相互排他のログ出力先にもこの優先順位が適用されます。`--log-file` を指定した場合は
設定ファイル側の `log-dir` を無視し、`--log-dir` を指定した場合は設定ファイル側の
`log-file` を無視します。

`log-dir` 利用時の保持期間 cleanup は、`otel-logger.YYYY-MM-DD` 形式かつ実在する暦日の日次ローテーション
ファイルだけを削除対象にします。同じディレクトリにある `otel-logger.pid`、
`otel-logger.stderr.log`、単体の `otel-logger.jsonl` などは削除しません。
`otel-logger.2026-99-99` のように日付として成立しない名前も削除対象外です。

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

注意: TOML 内のパスは一般的なシェル展開を行いません。上記のパス設定では先頭の
`~` / `~/` だけを展開しますが、`$HOME/logs` のような埋め込み環境変数は展開しません。
その場合は絶対パスを書いてください。

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
          --health-cmd "/usr/local/bin/otel-logger --dry-run --grpc-addr 127.0.0.1:0 --http-addr 127.0.0.1:0 --no-stdout"
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

## OTLP proxy 転送

受信した OTLP payload を **JSONL に保存しつつ** 上流の OTLP collector にも転送する
proxy モードがあります。Claude Code (Anthropic 系) と Codex (OpenAI 系) の 2 系統を
`service.name` で振り分け、それぞれ別の endpoint に送れます。

- **前提**: proxy を有効化するときは `--log-file` か `--log-dir` のどちらかを必ず指定する
  (転送に失敗しても受理済み payload を JSONL に残すため。上流への自動再送は Phase B で実装予定)
- **振り分け**: 組み込み既定で `claude-code` → Anthropic route、
  `codex_cli_rs` / `codex_exec` / `codex-app-server` → OpenAI route。
  config で `service_names` を明示すれば上書き可能
- **HTTP endpoint の検証**: `http-protobuf` route には絶対 `http://` / `https://`
  URL を指定する。設定値の末尾へ signal 別パス (`/v1/logs`、`/v1/traces`、
  `/v1/metrics`) を追加するため、query と fragment は startup 時に reject する
- **失敗時挙動**: JSONL 保存が成功してから proxy に `try_send` する fire-and-forget。
  route worker が指数バックオフで retry する (既定 8 回、200ms → 30s cap)。
  受信 endpoint は proxy の遅延に影響されない。shutdown 時は backoff 中だけでなく
  送信中の request も即座に中断し、設定した request timeout を待たない
- **認証**: header 値に `env:VAR_NAME` を書くと環境変数から解決する。secret を
  プロセス一覧や config ファイルに平文で残さないためこちらを推奨

### CLI 短縮フラグの例

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

### config file の例

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

`[[proxy.routes]]` は追加可能なので、社内 collector や staging 転送などを増やせます。
ただし異なる route が同じ `service.name` を主張するのは startup 時に error として reject
します (二重送信 / 順序不定を避けるため)。

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

`queue_depth` は snapshot 時点における route の bounded channel の実占有数です。
Tokio channel の capacity から直接算出するため、送受信が並行しても手動カウンタの
underflow や古い値は返しません。

### Phase B (今後の予定) — crash-safe outbox

現行 (Phase A) は「JSONL には確実に残るが、process crash 時に in-flight batch が転送
されない可能性がある」段階です。Phase B では JSONL の byte-offset を per-route
checkpoint として保持し、起動時に catch-up 走査して欠測ゼロを厳密に担保する予定です。
このため `--proxy-checkpoint-dir` フラグ・checkpoint ディレクトリの配置場所は先行して
用意されています。

## 累計トークン統計

`otel-logger` は両エージェントのトークン / コスト / duration を集計し、2 つの方法で公開します。
Claude は metrics で token/cost の初期値を集計し、対応する API request log が届いた場合はログ側へ置き換えたうえで request count/duration も補完します。
Codex の token usage は Codex が出す 2 つの形を重複排除して集計します。現在のローカルログと CI artifact では `codex.sse_event` / `response.completed` log が最も完全な token counter を持つため、これが最初の token source ならそれを採用し、`codex.turn.token_usage` metrics は最初または唯一の token source として観測された場合の fallback として使います。

| エージェント | tokens & cost                                              | request_count                       | duration                              | メタデータ                                                       |
|--------------|------------------------------------------------------------|-------------------------------------|---------------------------------------|------------------------------------------------------------------|
| claude-code  | metrics `claude_code.token.usage` + `claude_code.cost.usage` | log `claude_code.api_request`         | log `claude_code.api_request.duration_ms` | —                                                                |
| codex        | model ごとの最初の source: log `codex.sse_event` / `response.completed` または metric `codex.turn.token_usage` (Histogram、`total` は無視) | metric `codex.conversation.turn.count` | metric `codex.turn.e2e_duration_ms`     | log/span event `codex.conversation_starts` (`provider`/`effort` 補完) |

Anthropic のログは `model` のサフィックス (`[1m]` 等) を落とすため、メトリクス側で観測したフル名 (`claude-opus-4-7[1m]`) を canonical 表として保持し、後続のログ側 bare 名を同じ bucket にマージします。`aggregationTemporality=DELTA` のみ受け入れ、Cumulative は警告ログ付きで破棄します。

Codex は同じ usage を SSE 完了ログと turn token metrics の両方で送るため、`otel-logger` は model ごとに最初に観測した token source を採用し、その model ではもう一方の token counter を二重計上防止のため無視します。ローカル実ログでは SSE 先着と metric 先着の両方があり、metric 先着時も後着 SSE と token 種別ごとの合計が完全一致することを確認しています。`tool_token_count` は他の token 種別と重複するため加算しません。SSE ログの `cache_write_token_count` と metric の token type `cache_write_input` は、どちらも `cache_creation_tokens` に集計します。実ログでは `input_token_count == tool_token_count` かつ output/cache-read/cache-write/reasoning がすべて 0 の tool-only `response.completed` が turn metrics / `handle_responses` span usage から除外されているため、`otel-logger` でも token usage としては数えません。ただし、同じ形でも cache write が 1 以上なら実 usage として集計します。`session_task.turn` / `session_task.review` span の `codex.turn.token_usage.*` も同じ usage の別表現なので、trace span から token は計上しません。tracing exporter によっては span event の name が source location になり、論理名 `codex.conversation_starts` は `event.name` 属性へ格納されます。provider/effort の補完では、この現行形式と論理名を直接持つ旧形式の両方を受け付けます。log record では論理名が body、`event.name` 属性、top-level の `LogRecord.event_name` のいずれに入る形式も受け付けます。Codex token log が `conversation_starts` より先に届いた場合は、一時的な `effort=unknown` bucket を、後から届いた provider/model/effort bucket へ統合します。`conversation.id` が付与された SSE 完了ログは、対応する `codex.conversation_starts` メタデータにだけ紐付け、別 conversation の直近 session には決してフォールバックしません。メタデータがまだ届いていない場合は一旦 `effort=unknown` バケットに格納し、後から到着した時点で正しい effort バケットへ統合します。これにより、複数 conversation が混在したり、日をまたいだ継続セッションでも token usage が誤った effort バケットに移動しません。

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

bucket key は `provider/model/effort` 形式です。各 component 内の `/` と `%` は、それぞれ `%2F` / `%25` に percent-encode されるため、任意の telemetry label が別 bucket と衝突しません。Codex 側は ChatGPT / OpenAI が cost を出さないため `cost_usd` は常に `0` です。フラグ無しで常に取得できます。

### `--summary` (stdout、opt-in)

`--summary` (または `OTEL_LOGGER_SUMMARY=1` / 設定ファイルの `summary = true`) を有効にすると、使用量の累計が更新されるバッチを受信するたびに `[stats:<agent>]` ブロックが追記されます。

```
[stats:claude-code] requests=81 input=65509 output=85207 cache_read=8924351 cache_create=724182 reasoning=0 cost=$10.8716 duration=1262.400s since=2026-05-08T...
        breakdown provider=anthropic model=claude-opus-4-7[1m] effort=max: requests=74 input=1253 output=82097 cache_read=8924351 cache_create=671142 reasoning=0 cost=$10.7155 duration=1234.560s
        breakdown provider=anthropic model=claude-haiku-4-5-20251001 effort=unknown: requests=7 input=64256 output=3110 cache_read=0 cache_create=53040 reasoning=0 cost=$0.1561 duration=27.840s
```

カウンタはプロセス生存中の累計です。otel-logger を再起動するとリセットされます。

## 出力フォーマット

### stdout (pretty)

service 名、span 名、metric 名、severity text、属性キーなど、payload 由来の動的フィールドは escape してから表示します。

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
- `axum` がポート 4318 で `/v1/traces`、`/v1/metrics`、`/v1/logs` を受け、`application/x-protobuf` (prost デコード) と `application/json` (serde デコード) の両方に対応。`Content-Type` の media type は大小文字を区別せず判定するため (RFC 9110)、`Application/X-Protobuf; charset=utf-8` のような表記も受け付ける
- 両トランスポートとも 1 リクエストの decode 上限を 32 MiB (`OTLP_MAX_REQUEST_BYTES`) に引き上げ、大きな batch が 4 MiB / 2 MiB の既定値で恒久拒否されないようにする
- 両トランスポートが共通の `Sink` に流れ込み、stdout pretty と JSONL の両方へ書き出す
- `tokio_util::sync::CancellationToken` と SIGINT / SIGTERM を待つ `tokio::select!` で graceful shutdown。gRPC / HTTP task の終了を待ってから最後に JSONL を flush するため、末尾のバッチも欠落しません

## ライセンス

[MIT](LICENSE)
