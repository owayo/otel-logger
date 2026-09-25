# 設定

`otel-logger` が設定ファイルをどこから読み、CLI や環境変数の値とどう重ねるか、生成されるテンプレートに何が入るかをまとめます。オプションの一覧は [cli-reference.ja.md](cli-reference.ja.md) にあります。

## 置き場所

起動時に `$XDG_CONFIG_HOME/otel-logger/config.toml` (未設定なら `~/.config/otel-logger/config.toml`) を自動で読みます。`--config <PATH>` で別ファイルを指定可能。すべてのキーは任意で、無いキーはデフォルト値を使います。

`--config`、`otel-logger init --path`、`log-file`、`log-dir` では、先頭の `~` / `~/` を `$HOME` に展開します。環境変数や設定ファイル経由の値も同じ扱いです。

注意: TOML 内のパスは一般的なシェル展開を行いません。上記のパス設定では先頭の `~` / `~/` だけを展開しますが、`$HOME/logs` のような埋め込み環境変数は展開しません。その場合は絶対パスを書いてください。

## 優先順位

**優先順位** (上が強い): CLI フラグ > 環境変数 > 設定ファイル > 既定値。相互排他のログ出力先にもこの優先順位が適用されます。`--log-file` を指定した場合は設定ファイル側の `log-dir` を無視し、`--log-dir` を指定した場合は設定ファイル側の `log-file` を無視します。

## `log-dir` の保持期間

`log-dir` 利用時の保持期間 cleanup は、`otel-logger.YYYY-MM-DD` 形式かつ実在する暦日の日次ローテーションファイルだけを削除対象にします。同じディレクトリにある `otel-logger.pid`、`otel-logger.stderr.log`、単体の `otel-logger.jsonl` などは削除しません。`otel-logger.2026-99-99` のように日付として成立しない名前やシンボリックリンクも削除対象外です。

## テンプレート

コメント入りのテンプレートは `init` コマンドで生成できます:

```bash
otel-logger init                    # → ~/.config/otel-logger/config.toml
otel-logger init -p /etc/foo.toml   # → 任意のパス
otel-logger init -f                 # 既存ファイルを上書き
otel-logger init --daemon           # 常駐運用向けのプリセット
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

`--daemon` は常駐運用向けに調整した別のテンプレートを生成します (`no-stdout = true` と、保持設定付きの日次ローテーション JSONL)。詳細は [daemon.ja.md](daemon.ja.md) を参照してください。

```toml
# ~/.config/otel-logger/config.toml
log-dir = "/var/log/otel-logger"
log-keep-days = 10                 # 既定: 10
# 代わりに単一ファイルへ追記する場合 (`log-dir` と排他。ローテーションはされない):
# log-file = "/var/log/otel-logger/otel-logger.jsonl"
no-stdout = true
summary = false
color = "auto"  # "auto" | "always" | "never"
# grpc-addr = "0.0.0.0:4317"
# http-addr = "0.0.0.0:4318"
```

## proxy の route

OTLP proxy 転送は `[proxy]` テーブルと `[[proxy.routes]]` ブロックで設定します。キーと記述例は [proxy.ja.md](proxy.ja.md) を参照してください。
