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

## リクエストの上限

32 MiB の上限は 1 リクエスト分で、プロセス全体の上限ではありません。gzip の body は展開しながら最大 32 MiB まで膨らむため、接続を大量に開いて body を送り切らない client が 1 つあれば、数 KB の送信で数百 MB を抱えさせられます。そこで、同時に受信・保存する request の数と、受信にかけてよい時間を制限します。

- 上限は HTTP と gRPC、全 signal で 1 つを共有し、既定は同時 8 件 (`--max-inflight-requests`)。request は body を読む前 (gRPC は message を decode する前) に枠を取り、batch を保存して集計し終えるまで持つ。途中で client が切断しても、保存中なら枠は戻らない
- 上限を超えた request は、すぐには断らず空きを待つ。Codex は複数のプロセスから logs / traces / metrics を同時に送るので、瞬間的な重なりで断ると、retry しない exporter では batch が失われる
- 空きを待つ時間と body を読む時間の合計は、受け付けから 5 秒まで (`--receive-timeout-ms`)。OTLP exporter の既定 timeout 10 秒の半分にしている。保存と集計はこの期限で打ち切らない。保存した後で打ち切ると、保存済みの batch を exporter に再送させてしまう
- 期限に間に合わない request と、上限の 8 倍の件数が既に待っているときに来た request には、HTTP `503` / gRPC `Unavailable` を返す。どちらも OTLP exporter が retry する。`408` は OTLP では retry されないので使わない

## 人が読める出力 (stdout と `--pretty-log`)

- record ごとの同じ行と `--summary` のブロックを、stdout、`--pretty-log` のファイル (`<log-dir>/otel-logger.pretty.YYYY-MM-DD.log`)、その両方のいずれかへ書き出すか、どこにも出さない。2 つの出力先は互いに独立している ([daemon.ja.md](daemon.ja.md#人が読める出力を残す))
- 色は stdout にだけ付け、severity 別に色分けする。リダイレクト時や `NO_COLOR` の設定時は自動で色を付けない。pretty log のファイルには `color = "always"` でも ANSI の色コードを入れない
- 受信した payload の動的ラベルや属性キーに ANSI escape / C0/C1 制御文字が含まれていても、terminal や pretty log のファイルにそのまま出さず escape する (terminal escape injection 対策、JSONL は lossless のまま)
- 出力先ごとに専用の writer task と bounded queue (256 batch、整形後のテキストで 64 MiB) を持ち、OTLP の ACK 経路から切り離して書き込む。stdout の読み手が遅い場合 (pager を止めている、log driver が詰まっている等) でも応答や pretty log は止まらず、pretty log が詰まっても応答や stdout は止まらない。ACK を止めると exporter が timeout し、保存・計上済みの batch を再送させてしまうため
- 溢れた出力は破棄し、破棄した batch が 1、2、4、8、… 件に達するたびに warn する (JSONL 永続化・累計集計・proxy 転送には影響しない)。queue が空でも、整形後のテキストが単独で 64 MiB を超える batch は同じく破棄して件数に数える (escape で payload が数倍に膨らむことがあり、NUL バイト 1 つが 8 文字の `\u{0000}` になる)
- writer そのものが止まった場合は、最初に破棄した batch で、止まったことを error として知らせる。再起動するまで出力が戻らないので、「追いつかない」の warn とは分けて報告する。終了時も、writer の停止と drain の期限切れを別の警告にする
- 入れ子の属性値 (配列と key-value list) は、文字列ごとに 1 回だけ escape して描く。出力は入れ子の深さに比例して伸びるだけで済む。以前は描いた内側の値を層ごとに escape し直していたため、1 段ごとに長さが倍になり、数百 byte の深い入れ子の属性が数 GB に膨らんだ
- 書き込みの失敗 (ディスクが一杯など) を OTLP のエラーにはしない。pretty log と、broken pipe 以外の stdout のエラーについては、失敗が始まった時点で原因 (`No space left on device` など) を添えて 1 回報告し、失敗が続く間は最大 10 分に 1 回だけ再通知し、書き込みが 1 分間正常に続いた時点でもう 1 回知らせる
- 回復とみなすのは、最後の失敗から 60 秒以上たって書き込みが成功したときだけ。失敗が 1 分未満の間隔で繰り返される間は、合間に成功した書き込みがあっても 1 つの障害として扱う (開始の報告は 1 回で、その後は 10 分ごとの再通知)。空きがほとんど無いディスクで成功と失敗が交互に起きても、batch ごとに開始と回復の報告が対で出ることはない。回復を報告した後の失敗は、新しい障害としてすぐに報告する
- stdout の broken pipe (読み手がいなくなった) は stdout への出力だけを恒久的に止め、pretty log は書き続ける
- batch ごとに kernel まで flush するが、pretty log のファイルは終了時に `fsync` しない。電源断ではファイルの末尾が失われうる (欠落のない記録は JSONL が担う)
- pretty log のファイルには JSONL と同じ機微な属性が入るため、JSONL と同じく所有者のみアクセス可能な `--log-dir` ディレクトリで守る (`otel-logger` が作成した場合は `0700`)

## JSON Lines の永続化

- JSON Lines は単一の追記ファイルまたは日次ローテーションファイルへ保存する。日次ファイルは切り替え前に所有 handle で `fsync` し、graceful shutdown では現在のファイルを同期する。保管済みの過去ファイルは開き直さない
- JSONL の永続化に失敗した場合は HTTP `503 Service Unavailable` / gRPC `Status::unavailable` を返し、OTLP exporter 側に retry させる (受信 payload を黙って捨てない)。OTLP 仕様上 500 や `Internal` は retry されず破棄されるため、retryable な code を返す
- 累計使用量は JSONL 永続化に成功してから更新するため、retry された batch を二重計上しない
- 各 batch は改行まで kernel に書き渡してから ACK する。部分書き込みに失敗したら batch 開始位置へ truncate する。取り消しにも失敗した場合は、修復できるまで後続の書き込みを拒否する
- ファイルを開く際は、最後の改行より後の未完了 tail を除去してから追記する。この batch には ACK を返していない。同じ保存先を複数の受信プロセスから同時に書かない
- 日次 JSONL / pretty は書き込み時のローカル暦日で切り替える。数日間無通信でも次の書き込みを当日のファイルへ保存する
- 非有限の double (NaN、±Infinity) は、proto3 の JSON 表現と同じ文字列 `"NaN"` / `"Infinity"` / `"-Infinity"` で書く。素の `serde_json` は `null` を書き、値が失われる。有限値の出力は以前とバイト単位で同じ。`opentelemetry-proto` 0.33 はまだこの文字列を読み戻せないので、`replay_check` はその batch を decode 失敗として数える (上流: open-telemetry/opentelemetry-rust#3442)
- JSONL の障害は開始時に原因を報告し、続く失敗を最大 10 分ごとに再通知する。最後の失敗から 60 秒以上後の成功で回復を報告する
- JSONL・設定ファイル・`--log-dir` のディレクトリは所有者のみアクセス可能な権限 (`0600` / `0700`) で作成する。telemetry payload には `user.email` / `user.id` / organization ID が含まれるため

## 終了処理

- SIGINT / SIGTERM で新しい受信を止め、受理済みの書き込みが完了してから最終同期する。handler が cancel されても書き込み完了を待つ
- シグナルを受けたら、そのことをログに出す前にシャットダウンを始める。以前はログを先に出しており、stderr に書けないとシグナル処理の task がそこで終了した。`tokio` はシグナルの既定動作をプロセス全体で置き換えるので、以後の SIGTERM / SIGINT はすべて無視され、SIGKILL でしか止められなかった
- 接続の終了待ちには 10 秒の猶予を設ける。body を宣言したまま送り切らない client 1 本でプロセスを止められなくなり、最後の `fsync` に到達できない事態を防ぐ。2 回目のシグナルで in-flight を即座に諦める
- まず JSONL を flush して `fsync` し、続いて人が読める出力の 2 つの writer を、共通の 5 秒の期限内で並行して drain する。読み手が止まって stdout への書き込みがブロックしたままでも、プロセスの終了は妨げられない。最後の `fsync` に失敗しても drain は行い、queue に残った出力と最後の summary を捨てない
- stderr へ書き出し待ちの診断ログは、終了時に最大 1 秒だけ書き出しを待つ
- proxy の送信中・backoff 中だった batch と queue に残った batch は破棄せず 5 秒間の drain で送り切る。再起動のたびに「上流がまだ受け取っていない分」を無言で失わないため

10 秒は接続終了待ちの上限で、終了処理全体の上限ではありません。JSONL の disk 同期には timeout を掛けず、pretty / proxy の drain はそれぞれ期限を持ちます。強制終了や電源断は graceful shutdown の保証範囲外です。
