# 常駐運用

`otel-logger` を常駐サービスとして動かし続けるときに、出力を際限なく溜めないための設定です。

## `--no-stdout` が必須の理由

`otel-logger` を常駐させるときは、必ず `--no-stdout` を付けてください。人間向けの stdout 出力にはローテーションも上限もありません。stdout の fd 1 は親プロセス (launchd、systemd、コンテナランタイム) のもので、`otel-logger` には出力先のパスも保持の取り決めも分からないためです。`--log-keep-days` の保持設定が効くのは `otel-logger` が `--log-dir` に書く日次ファイル (JSONL と `--pretty-log` のファイル) **だけ**で、リダイレクトした stdout には一切適用されません。launchd の `StandardOutPath` も単純なシェルリダイレクトも出力先をローテーションしないため、実例では launchd で常駐させたインスタンスの stdout ファイルが 37 日で 7.6 GB (1 日あたり約 200 MB) まで膨らみ、誰も気づかないままディスクを消費していました。

launchd / systemd / Docker の下では stdout が端末になりません。`otel-logger` は起動時にこれを検出し、人間向け出力が有効なままなら stderr へ 1 回だけ警告を出します。警告は、無人で動かすなら `--no-stdout` (または設定ファイルの `no-stdout = true`) を、人が読める出力を残したいなら `--pretty-log` を足して `--log-dir` に日次ファイルとして書き出すことを勧めます。`--pretty-log` だけでは stdout への書き込みが続くため、警告は消えません。

`--no-stdout` は stdout への整形出力と `--summary` ブロックの両方を抑止しますが、累計集計そのものは動き続けます。最新の累計は `GET /stats` でいつでも取得できます (詳細は [usage-stats.ja.md](usage-stats.ja.md))。それでも人が読める出力を残したい場合は、`--pretty-log` でファイルに書き出します ([人が読める出力を残す](#人が読める出力を残す) を参照)。

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
# pretty-log = true                # 人が読める出力を日次ファイルにも書き出す (ディスクを余分に使う)
summary = false
color = "auto"  # "auto" | "always" | "never"
# grpc-addr = "0.0.0.0:4317"
# http-addr = "0.0.0.0:4318"
```

ディスクを余分に使うため、プリセットでは `pretty-log = true` をコメントアウトしています。有効にする前に次の節を確認してください。

## 人が読める出力を残す

`--pretty-log` (設定ファイルでは `pretty-log = true`、環境変数では `OTEL_LOGGER_PRETTY_LOG=1`) を付けると、人が読める出力を `--log-dir` 内の `otel-logger` 自身が管理するファイルへ書き出します。ローテーションと保持期間を `otel-logger` が制御できる場所です。ファイルの中身は stdout と同じ record ごとの行で、`--summary` が有効なら累計サマリーのブロックも入ります。

stdout とは別の出力先で、`--no-stdout` とは独立して働きます:

| `no-stdout` | `pretty-log` | 人が読める出力の行き先 |
|---|---|---|
| `false` | `false` | stdout |
| `false` | `true` | stdout と pretty log のファイル |
| `true` | `false` | どこにも出さない (累計集計と `GET /stats` は動き続ける) |
| `true` | `true` | pretty log のファイルだけ |

`--pretty-log` だけでは、リダイレクトした stdout の肥大化は止まりません。stdout への書き込みは続き、起動時の警告も出ます。常駐させるなら `--no-stdout --pretty-log` のように併用してください。

- **ファイル名**: `<log-dir>/otel-logger.pretty.YYYY-MM-DD.log`。ローカル時刻の日付で日次ローテーションし、切り替わりの境界は JSONL のファイル `otel-logger.YYYY-MM-DD` と同じ
- **`--log-dir` が必須**: 優先順位を解決した後の JSONL の出力先が `log-dir` である必要がある。実際の出力先が `log-file` の場合や、JSONL の出力先が 1 つも無い場合は起動時にエラーになる。pretty のファイルを `log-file` の隣などへ暗黙に書き出すことはない
- **保持期間**: `--log-keep-days` が日次の JSONL と同じ規則で pretty のファイルも削除する ([configuration.ja.md](configuration.ja.md#log-dir-の保持期間))。`--pretty-log` を無効に戻した後も、`--log-dir` を使っている限り、古い pretty のファイルは引き続き削除される
- **ディスク使用量**: ディスクを余分に使うため、既定では無効。上の launchd の例では、人が読める出力が 1 日あたり約 200 MB、JSONL が約 800 MB で、JSONL のおよそ 4 分の 1 の量が上乗せされる
- **色なし**: `--color` が効くのは stdout だけ。`color = "always"` でも pretty のファイルには ANSI の色コードを入れない。受信した payload の制御文字は stdout と同じく escape する
- **ベストエフォート**: stdout と同じく、pretty log も OTLP の ACK 経路から切り離し、専用の bounded queue を通して書き出す。溢れた分は破棄して報告し、書き込みの失敗 (ディスクが一杯など) を OTLP のエラーとして返すことはない。JSONL の永続化・累計集計・proxy 転送には影響しない。queue の上限と失敗の報告: [architecture.ja.md](architecture.ja.md)
- **`fsync` しない**: batch ごとに kernel まで flush するが、終了時に pretty のファイルは `fsync` しない。電源断ではファイルの末尾が失われうる。欠落のない記録は引き続き JSONL が担い、こちらは終了時に `fsync` する
- **権限**: pretty のファイルには JSONL と同じ機微な属性 (`user.email` など) が入る。日次の JSONL と同じく、ローテーション用のライブラリがプロセスの umask で作成するため、守っているのはディレクトリの権限になる。`otel-logger` が `--log-dir` を作成する場合は `0700` にし、既存のディレクトリの権限は変更しない
- **1 つのディレクトリに 1 インスタンス**: 複数の `otel-logger` で 1 つの `--log-dir` を共有する構成には対応しない

### 既存の launchd エージェントを移行する

`StandardOutPath` で人が読める出力をファイルへ書かせている既存のエージェントは、次の手順で移行します:

1. 設定ファイルで `log-dir` と合わせて `no-stdout = true` と `pretty-log = true` を指定します (または `ProgramArguments` の `--log-dir` と並べて `--no-stdout --pretty-log` を足します)。
2. 必要なら、下の例のように `StandardOutPath` を `/dev/null` に向けます。
3. エージェントを再起動します: `launchctl kickstart -k gui/$(id -u)/<label>`。`kickstart` は launchd が読み込み済みのジョブ定義のまま再起動するため、手順 1〜2 で plist を編集した場合は、代わりに `launchctl bootout` の後に `launchctl bootstrap` で読み込み直してください ([macOS (launchd)](#macos-launchd) を参照)。
4. 古い stdout のファイルは手で削除します。`otel-logger` は自分が作っていないファイルを削除しません。

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

人が読める出力を残す場合は、`--no-stdout` の直後に `<string>--pretty-log</string>` を足します。pretty のファイルは JSONL と同じ `--log-dir` に書き出されます。すでに stdout をファイルへ書かせているエージェントは [既存の launchd エージェントを移行する](#既存の-launchd-エージェントを移行する) を参照してください。

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

`/var/log/otel-logger` は unit の `User=` から書き込める必要があります。その中の JSONL は日次でローテーションされ、`--log-keep-days` を過ぎたものは削除されます。`ExecStart=` に `--pretty-log` を足すと、人が読める出力も同じディレクトリに日次の `otel-logger.pretty.YYYY-MM-DD.log` として残り、同じ設定で削除されます。

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

推奨は `otel-logger` 自身にファイルを管理させることです。`--log-dir` が日次でローテーションし、`--log-keep-days` が古いファイルを削除します。そのうえで `--no-stdout` で stdout から人間向けストリームを落とし、残したい場合は `--pretty-log` で日次ファイルとして書き出します。常駐インスタンスでどうしても stdout そのものを残したい場合は、rename 方式のローテータではなく、自前でローテーションする consumer (`multilog`、`rotatelogs`、journald など) にパイプしてください。
