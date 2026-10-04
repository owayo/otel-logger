# 内部構造

`otel-logger` がテレメトリを受信し、表示し、保存し、集計するまでの流れと、exporter の batch を失わず二重にも数えないために各段で守っていることをまとめます。

## 全体の流れ

- `tonic` が OTLP の 3 つの gRPC サービス (`TraceService` / `MetricsService` / `LogsService`) をポート 4317 で公開
- `axum` がポート 4318 で `/v1/traces`、`/v1/metrics`、`/v1/logs` を受け、`application/x-protobuf` (prost デコード) と `application/json` (serde デコード) の両方に対応。`Content-Type` の media type は大小文字を区別せず判定するため (RFC 9110)、`Application/X-Protobuf; charset=utf-8` のような表記も受け付ける
- 両トランスポートとも 1 リクエストの decode 上限を 32 MiB (`OTLP_MAX_REQUEST_BYTES`) に引き上げ、大きな batch が 4 MiB / 2 MiB の既定値で恒久拒否されないようにする
- `Content-Encoding: gzip` の body は decode 前に解凍する。32 MiB の上限は OTLP 仕様の要求どおり解凍後の body に対して効く
- 両トランスポートが共通の `Sink` に流れ込み、stdout pretty、任意の pretty log ファイル (`--pretty-log`)、JSONL へ書き出す
- `tokio_util::sync::CancellationToken` と SIGINT / SIGTERM を待つ `tokio::select!` で graceful shutdown。受理済みの書き込みは handler が cancel されても admission gate で完了を待ち、最後に JSONL を同期する

## 受信

- media type は大文字小文字を区別せず parameter 付きも受理する。非 UTF-8 の不正な `Content-Type` は protobuf へ暗黙フォールバックせず `415 Unsupported Media Type` を返す
- `Content-Encoding: gzip` の request も受け付ける (`OTEL_EXPORTER_OTLP_COMPRESSION=gzip` 対応)。サイズ上限は解凍後の body に対して効く
- gRPC / HTTP の 1 リクエスト上限を 32 MiB (`tonic` 既定 4 MiB / `axum` 既定 2 MiB より引き上げ) にし、大きな batch を `RESOURCE_EXHAUSTED` / `413` で恒久拒否せず保存する (exporter の retry でも回復できない欠落を防ぐ)

## 人が読める出力 (stdout と `--pretty-log`)

- record ごとの同じ行と `--summary` のブロックを、stdout、`--pretty-log` のファイル (`<log-dir>/otel-logger.pretty.YYYY-MM-DD.log`)、その両方のいずれかへ書き出すか、どこにも出さない。2 つの出力先は互いに独立している ([daemon.ja.md](daemon.ja.md#人が読める出力を残す))
- 色は stdout にだけ付け、severity 別に色分けする。リダイレクト時や `NO_COLOR` の設定時は自動で色を付けない。pretty log のファイルには `color = "always"` でも ANSI の色コードを入れない
- 受信した payload の動的ラベルや属性キーに ANSI escape / C0/C1 制御文字が含まれていても、terminal や pretty log のファイルにそのまま出さず escape する (terminal escape injection 対策、JSONL は lossless のまま)
- 出力先ごとに専用の writer task と bounded queue (256 batch、整形後のテキストで 64 MiB) を持ち、OTLP の ACK 経路から切り離して書き込む。stdout の読み手が遅い場合 (pager を止めている、log driver が詰まっている等) でも応答や pretty log は止まらず、pretty log が詰まっても応答や stdout は止まらない。ACK を止めると exporter が timeout し、保存・計上済みの batch を再送させてしまうため
- 溢れた出力は破棄し、破棄した batch が 1、2、4、8、… 件に達するたびに warn する (JSONL 永続化・累計集計・proxy 転送には影響しない)。queue が空でも、整形後のテキストが単独で 64 MiB を超える batch は同じく破棄して件数に数える (escape で payload が数倍に膨らむことがあり、NUL バイト 1 つが 8 文字の `\u{0000}` になる)
- 書き込みの失敗 (ディスクが一杯など) を OTLP のエラーにはしない。pretty log と、broken pipe 以外の stdout のエラーについては、失敗が始まった時点で原因 (`No space left on device` など) を添えて 1 回報告し、失敗が続く間は最大 10 分に 1 回だけ再通知し、書き込みが 1 分間正常に続いた時点でもう 1 回知らせる
- 回復とみなすのは、最後の失敗から 60 秒以上たって書き込みが成功したときだけ。失敗が 1 分未満の間隔で繰り返される間は、合間に成功した書き込みがあっても 1 つの障害として扱う (開始の報告は 1 回で、その後は 10 分ごとの再通知)。空きがほとんど無いディスクで成功と失敗が交互に起きても、batch ごとに開始と回復の報告が対で出ることはない。回復を報告した後の失敗は、新しい障害としてすぐに報告する
- stdout の broken pipe (読み手がいなくなった) は stdout への出力だけを恒久的に止め、pretty log は書き続ける
- batch ごとに kernel まで flush するが、pretty log のファイルは終了時に `fsync` しない。電源断ではファイルの末尾が失われうる (欠落のない記録は JSONL が担う)
- pretty log のファイルには JSONL と同じ機微な属性が入るため、JSONL と同じく所有者のみアクセス可能な `--log-dir` ディレクトリで守る (`otel-logger` が作成した場合は `0700`)

## JSON Lines の永続化

- JSON Lines は単一の追記ファイルまたは日次ローテーションファイルへ保存し、graceful shutdown 時に `fsync`
- JSONL の永続化に失敗した場合は HTTP `503 Service Unavailable` / gRPC `Status::unavailable` を返し、OTLP exporter 側に retry させる (受信 payload を黙って捨てない)。OTLP 仕様上 500 や `Internal` は retry されず破棄されるため、retryable な code を返す
- 累計使用量は JSONL 永続化に成功してから更新するため、retry された batch を二重計上しない
- 各 batch は改行まで kernel に書き渡してから ACK する。部分書き込みに失敗したら batch 開始位置へ truncate する。取り消しにも失敗した場合は、修復できるまで後続の書き込みを拒否する
- ファイルを開く際は、最後の改行より後の未完了 tail を除去してから追記する。この batch には ACK を返していない。同じ保存先を複数の受信プロセスから同時に書かない
- 日次 JSONL / pretty は書き込み時のローカル暦日で切り替える。数日間無通信でも次の書き込みを当日のファイルへ保存する
- JSONL の障害は開始時に原因を報告し、続く失敗を最大 10 分ごとに再通知する。最後の失敗から 60 秒以上後の成功で回復を報告する
- JSONL・設定ファイル・`--log-dir` のディレクトリは所有者のみアクセス可能な権限 (`0600` / `0700`) で作成する。telemetry payload には `user.email` / `user.id` / organization ID が含まれるため

## 終了処理

- SIGINT / SIGTERM で新しい受信を止め、受理済みの書き込みが完了してから最終同期する。handler が cancel されても書き込み完了を待つ
- 接続の終了待ちには 10 秒の猶予を設ける。body を宣言したまま送り切らない client 1 本でプロセスを止められなくなり、最後の `fsync` に到達できない事態を防ぐ。2 回目のシグナルで in-flight を即座に諦める
- まず JSONL を flush して `fsync` し、続いて人が読める出力の 2 つの writer を、共通の 5 秒の期限内で並行して drain する。読み手が止まって stdout への書き込みがブロックしたままでも、プロセスの終了は妨げられない
- proxy の送信中・backoff 中だった batch と queue に残った batch は破棄せず 5 秒間の drain で送り切る。再起動のたびに「上流がまだ受け取っていない分」を無言で失わないため

10 秒は接続終了待ちの上限で、終了処理全体の上限ではありません。JSONL の disk 同期には timeout を掛けず、pretty / proxy の drain はそれぞれ期限を持ちます。強制終了や電源断は graceful shutdown の保証範囲外です。
