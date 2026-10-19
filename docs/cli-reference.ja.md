# CLI リファレンス

`otel-logger` と `init` サブコマンドの全オプションです。設定ファイルのキーは [configuration.ja.md](configuration.ja.md) にまとめています。

## 書式

```bash
otel-logger [OPTIONS]
otel-logger init [--path <PATH>] [--config <PATH>] [--force] [--daemon]
```

サブコマンドを付けなければ OTLP 受信サーバとして起動します。

## オプション

| オプション      | 短縮形 | 既定値           | 環境変数                   | 説明                                                       |
|----------------|-------|------------------|---------------------------|------------------------------------------------------------|
| `--config`     |       | (自動)           | `OTEL_LOGGER_CONFIG`      | TOML 設定ファイルのパス ([configuration.ja.md](configuration.ja.md) を参照) |
| `--grpc-addr`  |       | `0.0.0.0:4317`   | `OTEL_LOGGER_GRPC_ADDR`   | gRPC バインドアドレス                                       |
| `--http-addr`  |       | `0.0.0.0:4318`   | `OTEL_LOGGER_HTTP_ADDR`   | HTTP バインドアドレス (protobuf / JSON 両対応)             |
| `--max-inflight-requests` | | `8` | `OTEL_LOGGER_MAX_INFLIGHT_REQUESTS` | 同時に受信・保存する request 数 (HTTP と gRPC の合計)。超えた分は `--receive-timeout-ms` まで空きを待ち、間に合わなければ retry できる `503` / `UNAVAILABLE` を返す ([architecture.ja.md](architecture.ja.md#リクエストの上限) を参照) |
| `--receive-timeout-ms` | | `5000` | `OTEL_LOGGER_RECEIVE_TIMEOUT_MS` | request を受け付けてから body を読み終えるまでの期限 (空きを待つ時間を含む)。保存の処理はこの期限で打ち切らない |
| `--log-file`   |       | (なし)           | `OTEL_LOGGER_LOG_FILE`    | 受信内容を JSON Lines で追記出力 (`--log-dir` と排他)        |
| `--log-dir`    |       | (なし)           | `OTEL_LOGGER_LOG_DIR`     | 指定ディレクトリに日次ローテーションで JSONL を出力 (`otel-logger.YYYY-MM-DD`、ローカルタイム) |
| `--log-keep-days` |    | `10`             | `OTEL_LOGGER_LOG_KEEP_DAYS` | `--log-dir` 利用時に日次ファイル (JSONL と `--pretty-log` のファイル) を保持する日数 (`0` を渡しても最低 1 日は残す) |
| `--no-stdout`  |       | `false`          | `OTEL_LOGGER_NO_STDOUT`   | 整形 stdout の出力を抑止 (常駐時は必須。[daemon.ja.md](daemon.ja.md) を参照)。`--pretty-log` と併用すると出力をファイルで残せる |
| `--pretty-log` |       | `false`          | `OTEL_LOGGER_PRETTY_LOG`  | 人が読める出力を `<log-dir>/otel-logger.pretty.YYYY-MM-DD.log` (日次、ローカルタイム) にも書き出す。色は付けない。`--log-dir` が必須 |
| `--summary`    |       | `false`          | `OTEL_LOGGER_SUMMARY`     | 使用量の累計が更新された時に、累計サマリーを人が読める出力 (stdout と `--pretty-log` のファイルのうち有効なもの) に追記 |
| `--color`      |       | `auto`           | `OTEL_LOGGER_COLOR`       | stdout の `auto` / `always` / `never` (`NO_COLOR` を尊重)。`--pretty-log` のファイルには色を付けない |
| `--dry-run`    | `-n`  | `false`          |                           | 両 listener の同時 bind と出力先を検証して終了。既存ログの修復・削除は行わない (未作成の出力先は作成)     |
| `--proxy-anthropic-endpoint` | | (なし) | `OTEL_LOGGER_PROXY_ANTHROPIC_ENDPOINT` | `service.name=claude-code` の受信 payload を転送する上流 OTLP endpoint (詳細は [proxy.ja.md](proxy.ja.md)) |
| `--proxy-anthropic-transport` | | `grpc` | `OTEL_LOGGER_PROXY_ANTHROPIC_TRANSPORT` | `grpc` / `http-protobuf`                                    |
| `--proxy-anthropic-header` | | (なし) | `OTEL_LOGGER_PROXY_ANTHROPIC_HEADERS` | `Key=Value` 形式 (`env:VAR_NAME` で環境変数解決)。複数指定可 |
| `--proxy-openai-endpoint` | | (なし) | `OTEL_LOGGER_PROXY_OPENAI_ENDPOINT` | Codex 系 (`codex_cli_rs` / `codex_exec` / `codex-app-server` / `codex_mcp_server` / `Codex Desktop` / `codex-exec-server`) の転送先 |
| `--proxy-openai-transport` | | `grpc` | `OTEL_LOGGER_PROXY_OPENAI_TRANSPORT` | 同上                                                        |
| `--proxy-openai-header` | | (なし) | `OTEL_LOGGER_PROXY_OPENAI_HEADERS` | 同上                                                        |
| `--proxy-checkpoint-dir` | | (JSONL の隣) | `OTEL_LOGGER_PROXY_CHECKPOINT_DIR` | 転送 checkpoint 用ディレクトリ (Phase B 用に予約)              |
| `--help`       | `-h`  |                  |                           | ヘルプ表示                                                 |
| `--version`    | `-V`  |                  |                           | バージョン表示                                             |

真偽値フラグ (`--no-stdout` / `--pretty-log` / `--summary`) は、コマンドラインでは `=` の後に値を明示でき (`--pretty-log=false`)、環境変数経由では `1` / `0`、`true` / `false`、`yes` / `no`、`on` / `off` のいずれの表記も受け付けます (systemd unit や compose の `OTEL_LOGGER_NO_STDOUT=1` がそのまま動きます)。CLI や環境変数で明示した `false` は設定ファイルの `true` に優先し、ドキュメント通りの優先順位になります。空文字の `OTEL_LOGGER_*` は「値が空」ではなく未設定として扱うため、`environment:` にプレースホルダを残しても起動を妨げません。

`OTEL_LOGGER_PROXY_*` の値は `--help` に表示しません (資格情報が入りうるため)。

## `init`

`otel-logger init` は、コメント入りの設定ファイルのテンプレートを受信サーバが読むパスへ書き出します。同じ `--config` / `OTEL_LOGGER_CONFIG` で起動した `otel-logger` は、そのファイルを読みます。既存のファイルは `--force` を付けない限り上書きしません。`--dry-run` と組み合わせた場合は、何も書かずにエラーで終了します。

| オプション | 短縮形 | 説明 |
|---|---|---|
| `--path` | `-p` | 出力先のパス。省略すると、受信サーバが読むパスへ書く。`--config` / `OTEL_LOGGER_CONFIG` があればそのパス、無ければ `$XDG_CONFIG_HOME/otel-logger/config.toml` (未設定なら `~/.config/otel-logger/config.toml`) |
| `--force` | `-f` | 既存のファイルを上書きする |
| `--daemon` | | 常駐運用向けのプリセット (`no-stdout = true` と日次ローテーションの JSONL。`pretty-log = true` はコメントアウトした状態で入る) を書き出す。書き出すのは設定ファイルだけで、サービス登録もバックグラウンド化も行わない |

```bash
otel-logger init                          # → ~/.config/otel-logger/config.toml
otel-logger init -p /etc/foo.toml         # → 任意のパス
otel-logger init --config /etc/foo.toml   # → /etc/foo.toml (`otel-logger --config /etc/foo.toml` が読むファイル)
otel-logger init -f                       # 既存ファイルを上書き
otel-logger init --daemon                 # 常駐運用向けのプリセット
```

## 診断ログ

サーバ自身の診断ログは **stderr** に出ます。`OTEL_LOGGER_LOG=debug` のように `tracing-subscriber` の env filter 文法でフィルタ可能です。

診断ログは専用のスレッドから書きます。stderr が読まれていない (pager を止めた、log driver が詰まった) ときや、書けない (ディスクが満杯、pipe が閉じた) ときでも、OTLP の応答と終了処理は止まりません。書けなかった行は捨てて数え、次に書けた行の前に件数を `WARN` の 1 行で知らせます。色は、stderr が端末で `NO_COLOR` が設定されていないときだけ付けます。
