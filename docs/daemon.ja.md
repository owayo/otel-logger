# 常駐運用

`otel-logger` を常駐サービスとして動かし続けるときに、出力を際限なく溜めないための設定です。

## `--no-stdout` が必須の理由

`otel-logger` を常駐させるときは、必ず `--no-stdout` を付けてください。人間向けの stdout 出力にはローテーションも上限もありません。`--log-keep-days` の保持設定が効くのは `--log-dir` が書く JSONL **だけ**で、リダイレクトした stdout には一切適用されません。launchd の `StandardOutPath` も単純なシェルリダイレクトも出力先をローテーションしないため、実例では launchd で常駐させたインスタンスの stdout ファイルが 37 日で 7.6 GB (1 日あたり約 200 MB) まで膨らみ、誰も気づかないままディスクを消費していました。

launchd / systemd / Docker の下では stdout が端末になりません。`otel-logger` は起動時にこれを検出し、人間向け出力が有効なままなら stderr へ 1 回だけ警告を出して `--no-stdout` (または設定ファイルの `no-stdout = true`) を促します。

`--no-stdout` は整形出力と `--summary` ブロックの両方を抑止しますが、累計集計そのものは動き続けます。常駐中の状況確認はログを読むのではなく `GET /stats` を叩いてください (詳細は [usage-stats.ja.md](usage-stats.ja.md))。

常駐向けの既定値が入った設定ファイルは `--daemon` プリセットで生成できます。生成するのは設定ファイルだけで、サービス登録もバックグラウンド化も行いません:

```bash
otel-logger init --daemon                             # → ~/.config/otel-logger/config.toml
otel-logger init --daemon -p /etc/otel-logger/config.toml
```

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

## macOS (launchd)

`~/Library/LaunchAgents/io.github.owayo.otel-logger.plist` として保存します (`YOUR_USER` はログディレクトリを所有するアカウント名に置き換えてください):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>io.github.owayo.otel-logger</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/otel-logger</string>
    <string>--no-stdout</string>
    <string>--log-dir</string>
    <string>/Users/YOUR_USER/Library/Logs/otel-logger</string>
    <string>--log-keep-days</string>
    <string>10</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>/dev/null</string>
  <key>StandardErrorPath</key>
  <string>/dev/null</string>
</dict>
</plist>
```

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.github.owayo.otel-logger.plist
launchctl print gui/$(id -u)/io.github.owayo.otel-logger          # 状態確認
launchctl kickstart -k gui/$(id -u)/io.github.owayo.otel-logger   # 再起動
launchctl bootout gui/$(id -u)/io.github.owayo.otel-logger        # 停止してアンロード
```

`StandardOutPath` を `/dev/null` にしているのは、launchd がストリームをリダイレクトするだけで出力先ファイルをローテーションしないためです。`StandardErrorPath` も同じで、サーバ自身の診断ログを残すために実ファイルを指定した場合、そのファイルのローテーションと削除は launchd ではなく利用者側の責任になります。

## Linux (systemd)

`/etc/systemd/system/otel-logger.service` として保存します:

```ini
[Unit]
Description=otel-logger OTLP receiver
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=otel-logger
ExecStart=/usr/local/bin/otel-logger --no-stdout --log-dir /var/log/otel-logger --log-keep-days 10
Restart=on-failure
RestartSec=5s
StandardOutput=null
StandardError=journal

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now otel-logger.service
sudo systemctl status otel-logger.service
journalctl -u otel-logger.service -f
```

`/var/log/otel-logger` は unit の `User=` から書き込める必要があります。その中の JSONL は日次でローテーションされ、`--log-keep-days` を過ぎたものは削除されます。

2 つの出力ストリームは上限のかかり方が違うので、区別して考えてください。`StandardError=journal` はサーバの診断ログを journald に渡すため、ホスト側の保持設定 (`journald.conf` の `SystemMaxUse=` や `MaxRetentionSec=` など) で管理され、それ自体で上限が付きます。一方 `StandardOutput=append:/var/log/otel-logger/stdout.log` のようにファイルへ直接追記する指定は何もローテーションしないため、際限なく増えるのはこちらです。

## Docker のログドライバ

既定の `json-file` ログドライバはローテーションしないため、常駐させたコンテナは stdout も stderr もコンテナの寿命の分だけ溜め続けます。`--no-stdout` で大半は消えますが、サーバ自身の診断ログは stderr に残ります。コンテナログを残すなら上限を明示してください。既定でローテーションする `local` ドライバを使うか、`json-file` に同じオプションを与えます:

```yaml
services:
  otel-logger:
    image: ghcr.io/owayo/otel-logger:latest
    command: ["--no-stdout", "--log-dir", "/var/log/otel-logger", "--log-keep-days", "10"]
    volumes:
      - ./data:/var/log/otel-logger
    logging:
      driver: local        # json-file に同じ 2 つのオプションを与えてもよい
      options:
        max-size: "10m"
        max-file: "3"
```

同梱の [`compose.yaml`](../compose.yaml) は、短時間のサンプル実行で `docker compose logs otel-logger` が読めるように、あえて stdout を残しています。常駐させる場合は `--no-stdout` と `logging:` ブロックを足してください。

## 外部ローテータ (newsyslog / logrotate)

`otel-logger` は `SIGHUP` による出力ファイルの再オープンに対応していません。そのため「rename してシグナル」という一般的なローテーション方式は使えません。`logrotate` や `newsyslog` がファイルを rename した後もプロセスは古い fd に書き続けるため、新しいファイルは空のままで、ディスク容量も解放されません。`copytruncate` は rename を避けられますが、コピーと truncate の間に書かれた分を失いうるので、JSONL の出力先に指定してはいけません。

推奨は `otel-logger` 自身にファイルを管理させることです。`--log-dir` が日次でローテーションし、`--log-keep-days` が古いファイルを削除します。そのうえで `--no-stdout` で人間向けストリームを落とします。常駐インスタンスでどうしても stdout を残したい場合は、rename 方式のローテータではなく、自前でローテーションする consumer (`multilog`、`rotatelogs`、journald など) にパイプしてください。
