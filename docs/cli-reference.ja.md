# CLI リファレンス

`otel-logger` と `init` サブコマンドの全オプションです。設定ファイルのキーは [configuration.ja.md](configuration.ja.md) にまとめています。

## 書式

```bash
otel-logger [OPTIONS]
otel-logger init [--path <PATH>] [--force] [--daemon]
```

サブコマンドを付けなければ OTLP 受信サーバとして起動します。

## オプション

| オプション      | 短縮形 | 既定値           | 環境変数                   | 説明                                                       |
|----------------|-------|------------------|---------------------------|------------------------------------------------------------|
| `--config`     |       | (自動)           | `OTEL_LOGGER_CONFIG`      | TOML 設定ファイルのパス ([configuration.ja.md](configuration.ja.md) を参照) |
| `--grpc-addr`  |       | `0.0.0.0:4317`   | `OTEL_LOGGER_GRPC_ADDR`   | gRPC バインドアドレス                                       |
| `--http-addr`  |       | `0.0.0.0:4318`   | `OTEL_LOGGER_HTTP_ADDR`   | HTTP バインドアドレス (protobuf / JSON 両対応)             |
| `--log-file`   |       | (なし)           | `OTEL_LOGGER_LOG_FILE`    | 受信内容を JSON Lines で追記出力 (`--log-dir` と排他)        |
| `--log-dir`    |       | (なし)           | `OTEL_LOGGER_LOG_DIR`     | 指定ディレクトリに日次ローテーションで JSONL を出力 (`otel-logger.YYYY-MM-DD`、ローカルタイム) |
| `--log-keep-days` |    | `10`             | `OTEL_LOGGER_LOG_KEEP_DAYS` | `--log-dir` 利用時に保持する日数 (`0` を渡しても最低 1 日は残す) |
| `--no-stdout`  |       | `false`          | `OTEL_LOGGER_NO_STDOUT`   | 整形 stdout の出力を抑止 (常駐時は必須。[daemon.ja.md](daemon.ja.md) を参照) |
| `--summary`    |       | `false`          | `OTEL_LOGGER_SUMMARY`     | 使用量の累計が更新された時に累計サマリーを stdout に追記       |
| `--color`      |       | `auto`           | `OTEL_LOGGER_COLOR`       | `auto` / `always` / `never` (`NO_COLOR` を尊重)            |
| `--dry-run`    | `-n`  | `false`          |                           | 両 listener の同時 bind を含む起動チェックを実施して終了     |
| `--proxy-anthropic-endpoint` | | (なし) | `OTEL_LOGGER_PROXY_ANTHROPIC_ENDPOINT` | `service.name=claude-code` の受信 payload を転送する上流 OTLP endpoint (詳細は [proxy.ja.md](proxy.ja.md)) |
| `--proxy-anthropic-transport` | | `grpc` | `OTEL_LOGGER_PROXY_ANTHROPIC_TRANSPORT` | `grpc` / `http-protobuf`                                    |
| `--proxy-anthropic-header` | | (なし) | `OTEL_LOGGER_PROXY_ANTHROPIC_HEADERS` | `Key=Value` 形式 (`env:VAR_NAME` で環境変数解決)。複数指定可 |
| `--proxy-openai-endpoint` | | (なし) | `OTEL_LOGGER_PROXY_OPENAI_ENDPOINT` | Codex 系 (`codex_cli_rs` / `codex_exec` / `codex-app-server` / `codex_mcp_server`) の転送先 |
| `--proxy-openai-transport` | | `grpc` | `OTEL_LOGGER_PROXY_OPENAI_TRANSPORT` | 同上                                                        |
| `--proxy-openai-header` | | (なし) | `OTEL_LOGGER_PROXY_OPENAI_HEADERS` | 同上                                                        |
| `--proxy-checkpoint-dir` | | (JSONL の隣) | `OTEL_LOGGER_PROXY_CHECKPOINT_DIR` | 転送 checkpoint 用ディレクトリ (Phase B 用に予約)              |
| `--help`       | `-h`  |                  |                           | ヘルプ表示                                                 |
| `--version`    | `-V`  |                  |                           | バージョン表示                                             |

真偽値フラグ (`--no-stdout` / `--summary`) は、環境変数経由で `1` / `0`、`true` / `false`、`yes` / `no`、`on` / `off` のいずれの表記も受け付けます (systemd unit や compose の `OTEL_LOGGER_NO_STDOUT=1` がそのまま動きます)。明示的な `false` は設定ファイルの `true` に優先し、ドキュメント通りの優先順位になります。空文字の `OTEL_LOGGER_*` は「値が空」ではなく未設定として扱うため、`environment:` にプレースホルダを残しても起動を妨げません。

`OTEL_LOGGER_PROXY_*` の値は `--help` に表示しません (資格情報が入りうるため)。

## `init`

`otel-logger init` はコメント入りの設定ファイルのテンプレートを書き出します。既存のファイルは `--force` を付けない限り上書きしません。

| オプション | 短縮形 | 説明 |
|---|---|---|
| `--path` | `-p` | 出力先のパス (既定は受信サーバが読むパスと同じ `$XDG_CONFIG_HOME/otel-logger/config.toml`、未設定なら `~/.config/otel-logger/config.toml`) |
| `--force` | `-f` | 既存のファイルを上書きする |
| `--daemon` | | 常駐運用向けのプリセット (`no-stdout = true` と日次ローテーションの JSONL) を書き出す。書き出すのは設定ファイルだけで、サービス登録もバックグラウンド化も行わない |

```bash
otel-logger init                    # → ~/.config/otel-logger/config.toml
otel-logger init -p /etc/foo.toml   # → 任意のパス
otel-logger init -f                 # 既存ファイルを上書き
otel-logger init --daemon           # 常駐運用向けのプリセット
```

## 診断ログ

サーバ自身の診断ログは **stderr** に出ます。`OTEL_LOGGER_LOG=debug` のように `tracing-subscriber` の env filter 文法でフィルタ可能です。
