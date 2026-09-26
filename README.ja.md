<h1 align="center">otel-logger</h1>

<p align="center">
  Claude Code / Codex の OTLP テレメトリを stdout と JSON Lines で記録する受信サーバ
</p>

<!-- standard:badges:start -->
<h3 align="center">対応プラットフォーム</h3>

<p align="center">
  <img src="https://img.shields.io/badge/Linux-FCC624?logo=linux&amp;logoColor=black" alt="Linux">
  <img src="https://img.shields.io/badge/macOS-000000?logo=apple&amp;logoColor=white" alt="macOS">
  <img src="https://img.shields.io/badge/Windows-0078D6" alt="Windows">
</p>

<p align="center">
  <a href="https://github.com/owayo/otel-logger/actions/workflows/ci.yml"><img src="https://github.com/owayo/otel-logger/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/owayo/otel-logger/releases/latest"><img src="https://img.shields.io/github/v/release/owayo/otel-logger" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/owayo/otel-logger" alt="License"></a>
</p>

<p align="center">
  <a href="README.md">English</a> |
  <a href="README.ja.md">日本語</a>
</p>
<!-- standard:badges:end -->

---

`otel-logger` は CI コンテナ上で動く AI コーディングエージェント (**Claude Code** / **OpenAI Codex CLI**) の隣に置く、Rust 製の小さな OTLP 受信サーバです。OTLP/gRPC を `:4317`、OTLP/HTTP を `:4318` で受け、Traces / Metrics / Logs をデコードして 2 つの経路に出力します。

- **stdout**: 1 件ずつ整形した可読ログ (CI ログでそのまま読める)
- **JSON Lines** (`--log-file` / `--log-dir`): 元の OTLP 構造を欠落させない永続化先。単一の追記ファイルまたは日次ローテーションファイルとして保存

既定では Jaeger や Honeycomb への転送を行いません。CI 実行中にエージェントが吐くテレメトリを、開発者がいつも見ている場所 (CI ログ / アーティファクト) に出すことが主目的です。任意の OTLP proxy route を設定すると、永続化した payload を上流 collector にも転送できます。

## 機能

- **OTLP の 2 経路を 1 プロセスで受信**: OTLP/gRPC (4317) と OTLP/HTTP (4318)。HTTP は `application/x-protobuf` と `application/json` の両方を、gzip 圧縮の有無を問わず受け付ける。1 リクエストの上限 32 MiB は解凍後のサイズで判定する
- **読みやすい stdout**: severity 別に色分けする (リダイレクト時や `NO_COLOR` の設定時は色を付けない)。受信した payload の制御文字は terminal に届く前に escape する
- **欠落のない JSON Lines**: 単一の追記ファイルか日次ローテーションのファイルに保存し、graceful shutdown 時に `fsync` する。書き込みに失敗したら HTTP `503` / gRPC `Unavailable` を返し、exporter に batch を捨てさせず再送させる
- **Claude Code と Codex の累計使用量**: token・コスト・所要時間を provider/model/effort ごとに累計し、`GET /stats` と `--summary` で出す。logs と metrics の二重計上はしない
- **OTLP proxy 転送**: 保存した Claude Code / Codex の payload を、それぞれ別の上流 collector へ転送できる (任意)
- **graceful shutdown**: SIGINT / SIGTERM で末尾の batch まで書き切る (`docker stop` でも落ちない)。待つのは最大 10 秒
- **所有者だけが読めるファイル**: JSONL・設定ファイル・`--log-dir` のディレクトリを `0600` / `0700` で作る。telemetry には `user.email`・`user.id`・organization ID が含まれるため
- **小さな配布物**: stripped で約 7 MB の単一バイナリ、glibc の無い環境 (distroless、Alpine) 向けの musl 静的ビルド、distroless のコンテナイメージ
- **すぐ使える利用例**: Docker Compose / GitHub Actions / GitLab CI

設計の詳細 (リクエストの上限、stdout の writer、配送の保証): [docs/architecture.ja.md](docs/architecture.ja.md)

## インストール

<!-- standard:install:start -->
### Homebrew (macOS/Linux)

```bash
brew install owayo/otel-logger/otel-logger
```

### Cargo

Rust 1.98 以上が必要です。

```bash
cargo install --git https://github.com/owayo/otel-logger --locked
```

### GitHub Releases から

[Releases](https://github.com/owayo/otel-logger/releases/latest) から自分の環境のアーカイブを取得して展開し、`otel-logger` を `PATH` の通った場所に置きます。各リリースには、取得したファイルを確かめるための `SHA256SUMS` も添付しています。

| プラットフォーム | ファイル |
|---|---|
| Linux (x86_64) | `otel-logger-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (x86_64, musl) | `otel-logger-x86_64-unknown-linux-musl.tar.gz` |
| Linux (ARM64) | `otel-logger-aarch64-unknown-linux-gnu.tar.gz` |
| macOS (Intel) | `otel-logger-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `otel-logger-aarch64-apple-darwin.tar.gz` |
| Windows (x86_64) | `otel-logger-x86_64-pc-windows-msvc.zip` |

macOS でブラウザから取得した場合は、実行の前に隔離属性を外します: `xattr -d com.apple.quarantine otel-logger`。

### ソースから

[mise](https://mise.jdx.dev/) が必要です (Rust のツールチェーンは `mise.toml` で固定しています)。

```bash
git clone https://github.com/owayo/otel-logger.git
cd otel-logger
make install
```

`make install` は `/usr/local/bin` に入れます。場所を変えるときは `INSTALL_PATH` を指定します (例: `make install INSTALL_PATH="$HOME/.local/bin"`)。
<!-- standard:install:end -->

### Docker

同梱の Dockerfile (非 root ユーザーで動く distroless イメージ) をビルドし、OTLP の標準ポートで起動します。

```bash
docker build -t otel-logger:dev .
docker run --rm -p 4317:4317 -p 4318:4318 otel-logger:dev
```

## 使い方

```bash
otel-logger [OPTIONS]
```

```bash
# 標準ポートで起動し、JSONL もディスクに残す
otel-logger --log-file ./otel.jsonl

# HTTP のみ動かしたい (gRPC は使わないアドレスへ)
otel-logger --grpc-addr 127.0.0.1:0 --http-addr 0.0.0.0:4318

# CI でのスモークテスト
otel-logger --dry-run
```

全オプションと対応する環境変数、`init` サブコマンド: [docs/cli-reference.ja.md](docs/cli-reference.ja.md)

### Claude Code からの送信

Claude Code は環境変数で OTLP exporter を有効化します。詳細は [Anthropic monitoring ドキュメント](https://code.claude.com/docs/en/monitoring-usage) を参照してください。毎回 export せずに済ませたい場合は `~/.claude/settings.json` (またはプロジェクト直下の `.claude/settings.json`) の `env` ブロックに書くのが手軽です。

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

### OpenAI Codex CLI からの送信

Codex は環境変数ではなく `config.toml` の `[otel]` セクションが公式契約です。詳細は [Codex config reference](https://developers.openai.com/codex/config-reference) を参照してください。

```toml
# $CODEX_HOME/config.toml
[otel]
environment = "local"
log_user_prompt = false # プロンプトの内容は送信しない
exporter = { otlp-http = { endpoint = "http://localhost:4318/v1/logs", protocol = "binary", headers = {} } }
metrics_exporter = { otlp-http = { endpoint = "http://localhost:4318/v1/metrics", protocol = "binary", headers = {} } }
trace_exporter = { otlp-http = { endpoint = "http://localhost:4318/v1/traces", protocol = "binary", headers = {} } }
```

Docker Compose / CI で同じネットワークの受信コンテナへ送る場合は、`localhost` をサービス名 (`otel-logger`) に置き換えてください。そのまま使えるサンプルを [`codex-config/config.toml`](codex-config/config.toml) に同梱しています。

Docker Compose・GitHub Actions・GitLab CI で受信サーバをサイドカーとして動かす例: [docs/integrations.ja.md](docs/integrations.ja.md)

### 累計使用量

`GET /stats` は、Claude/Codex の累計使用量 (token・コスト・所要時間) を provider/model/effort ごとに JSON で常に返します。`--summary` を付けると、batch で累計が変わるたびに同じ値を stdout にも追記します。token/cost は metrics より Claude の API request ログを優先し、対応する metrics は二重に加算しません。

```bash
curl -s http://localhost:4318/stats | jq
```

`/stats` に認証はありません。OTLP/HTTP と同じ listener で提供され、既定の bind は `0.0.0.0:4318` なので、数値を秘匿したい場合は `127.0.0.1` に bind するか、ファイアウォールでポートを制限してください。エージェントごとの集計方法とレスポンスの形式: [docs/usage-stats.ja.md](docs/usage-stats.ja.md)

### 出力フォーマット

service 名、span 名、metric 名、severity text、属性キーなど、payload 由来の動的フィールドは escape してから表示します。

```text
[trace]  2026-05-07T22:01:14.123Z service=claude-code scope=anthropic.claude_code span=tool.call dur=812ms status=OK trace=4d2... span_id=8ab...
        attrs: {tool.name=Bash, exit_code=0}
[log]    2026-05-07T22:01:14.456Z service=claude-code scope=anthropic.claude_code severity=INFO body="ran command"
[metric] service=claude-code scope=anthropic.claude_code name=claude_code.tokens.input sum=[1234 {model=claude-sonnet-4-6}]
```

JSON Lines のファイルは 1 行 1 JSON で、OTLP の元構造をそのまま保持しています (キーは OTLP/JSON の camelCase です)。`jq` や任意のデータウェアハウスに流し込んで分析できます。

```json
{"kind":"traces","resourceSpans":[{"resource":{"attributes":[…]},"scopeSpans":[…]}]}
{"kind":"metrics","resourceMetrics":[…]}
{"kind":"logs","resourceLogs":[…]}
```

## 設定

起動時に `$XDG_CONFIG_HOME/otel-logger/config.toml` (未設定なら `~/.config/otel-logger/config.toml`) を自動で読みます。`--config <PATH>` で別のファイルを指定できます。すべてのキーは任意で、無いキーは既定値を使います。優先順位は、強い順に CLI フラグ > 環境変数 > 設定ファイル > 既定値です。

コメント入りのテンプレートは `otel-logger init` で生成できます (`--daemon` を付けると常駐運用向けのプリセットになります)。

```bash
otel-logger init
```

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

パスの展開、`log-dir` の保持期間の規則、常駐向けのプリセット: [docs/configuration.ja.md](docs/configuration.ja.md)

## 常駐運用

常駐させるなら `--no-stdout` は必須です。otel-logger は stdout をローテーションせず、`--log-keep-days` が削除するのは `--log-dir` が書いた JSONL だけです。launchd で常駐させたインスタンスの stdout ファイルが、誰も気づかないまま 37 日で 7.6 GB まで膨らんだ例があります。stdout が端末でないのに人間向けの出力が有効なままなら、起動時に stderr へ警告を出します。

launchd と systemd の設定例、Docker のログドライバ、rename 方式のローテータが使えない理由: [docs/daemon.ja.md](docs/daemon.ja.md)

## OTLP proxy 転送

受信した OTLP payload を **JSONL に保存しつつ** 上流の OTLP collector にも転送できます。Claude Code と Codex の通信は `service.name` で振り分け、それぞれ別の endpoint に送れます。転送するには JSONL の出力先 (`--log-file` か `--log-dir`) を指定します。上流が遅くても受信は止まりません。

```bash
otel-logger \
  --log-file ./otel.jsonl \
  --proxy-anthropic-endpoint https://collector.example.com:4317 \
  --proxy-anthropic-header 'Authorization=env:ANTHROPIC_PROXY_TOKEN'
```

振り分け、再送、TOML での書き方、route ごとの送信件数: [docs/proxy.ja.md](docs/proxy.ja.md)

## 開発

<!-- standard:dev:start -->
[mise](https://mise.jdx.dev/) が必要です。ツールの版は `mise.toml` で固定しています。

```bash
make setup   # ツールチェーン (mise) と依存を取得する
make ci      # CI と同じ検査 (書き換えない)
```

| コマンド | 説明 |
|---|---|
| `make setup` | ツールチェーン (mise) と依存を取得する |
| `make build` | デバッグ版をビルドする |
| `make release` | リリース版をビルドする |
| `make run` | デバッグ版を実行する (引数は ARGS="...") |
| `make test` | テストを実行する |
| `make lint` | clippy を警告ゼロで通す |
| `make fmt` | コードを整形する (書き換える) |
| `make fmt-check` | 整形済みかを確かめる (書き換えない) |
| `make check` | 整形と静的検査 (書き換えない) |
| `make ci` | CI と同じ検査 (書き換えない) |
| `make install` | リリース版を INSTALL_PATH (既定 /usr/local/bin) に入れる |
| `make uninstall` | INSTALL_PATH から取り除く |
| `make clean` | ビルド成果物を消す |

`make` でターゲットの一覧を表示します。リリースは GitHub Actions で行います (**Actions → Release → Run workflow**)。
<!-- standard:dev:end -->

保存した JSONL を集計器で再生する手順、依存の監査、Docker のターゲット: [docs/development.ja.md](docs/development.ja.md)

## ライセンス

<!-- standard:license:start -->
[MIT](LICENSE)
<!-- standard:license:end -->
