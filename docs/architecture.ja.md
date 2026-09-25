# 内部構造

`otel-logger` がテレメトリを受信し、表示し、保存し、集計するまでの流れと、exporter の batch を失わず二重にも数えないために各段で守っていることをまとめます。

## 全体の流れ

- `tonic` が OTLP の 3 つの gRPC サービス (`TraceService` / `MetricsService` / `LogsService`) をポート 4317 で公開
- `axum` がポート 4318 で `/v1/traces`、`/v1/metrics`、`/v1/logs` を受け、`application/x-protobuf` (prost デコード) と `application/json` (serde デコード) の両方に対応。`Content-Type` の media type は大小文字を区別せず判定するため (RFC 9110)、`Application/X-Protobuf; charset=utf-8` のような表記も受け付ける
- 両トランスポートとも 1 リクエストの decode 上限を 32 MiB (`OTLP_MAX_REQUEST_BYTES`) に引き上げ、大きな batch が 4 MiB / 2 MiB の既定値で恒久拒否されないようにする
- `Content-Encoding: gzip` の body は decode 前に解凍する。32 MiB の上限は OTLP 仕様の要求どおり解凍後の body に対して効く
- 両トランスポートが共通の `Sink` に流れ込み、stdout pretty と JSONL の両方へ書き出す
- `tokio_util::sync::CancellationToken` と SIGINT / SIGTERM を待つ `tokio::select!` で graceful shutdown。gRPC / HTTP task の終了を待ってから最後に JSONL を flush するため、末尾のバッチも欠落しません

## 受信

- media type は大文字小文字を区別せず parameter 付きも受理する。非 UTF-8 の不正な `Content-Type` は protobuf へ暗黙フォールバックせず `415 Unsupported Media Type` を返す
- `Content-Encoding: gzip` の request も受け付ける (`OTEL_EXPORTER_OTLP_COMPRESSION=gzip` 対応)。サイズ上限は解凍後の body に対して効く
- gRPC / HTTP の 1 リクエスト上限を 32 MiB (`tonic` 既定 4 MiB / `axum` 既定 2 MiB より引き上げ) にし、大きな batch を `RESOURCE_EXHAUSTED` / `413` で恒久拒否せず保存する (exporter の retry でも回復できない欠落を防ぐ)

## stdout の出力

- severity 別に色分けし、リダイレクト時や `NO_COLOR` の設定時は自動で色を付けない
- 受信した payload の動的ラベルや属性キーに ANSI escape / C0/C1 制御文字が含まれていても terminal にそのまま出さず escape する (terminal escape injection 対策、JSONL は lossless のまま)
- stdout への書き込みは OTLP の ACK 経路から切り離した専用 writer task が行う。読み手が遅い場合 (pager を止めている、log driver が詰まっている等) でも応答が止まらない。ACK を止めると exporter が timeout し、保存・計上済みの batch を再送させてしまうため。溢れた出力は破棄して件数を warn する (JSONL 永続化・累計集計・proxy 転送には影響しない)

## JSON Lines の永続化

- JSON Lines は単一の追記ファイルまたは日次ローテーションファイルへ保存し、graceful shutdown 時に `fsync`
- JSONL の永続化に失敗した場合は HTTP `503 Service Unavailable` / gRPC `Status::unavailable` を返し、OTLP exporter 側に retry させる (受信 payload を黙って捨てない)。OTLP 仕様上 500 や `Internal` は retry されず破棄されるため、retryable な code を返す
- 累計使用量は JSONL 永続化に成功してから更新するため、retry された batch を二重計上しない
- 各 batch は ACK 前に `BufWriter::flush` で kernel まで書き出すため、process crash で末尾の write がメモリバッファに取り残されることがない
- JSONL・設定ファイル・`--log-dir` のディレクトリは所有者のみアクセス可能な権限 (`0600` / `0700`) で作成する。telemetry payload には `user.email` / `user.id` / organization ID が含まれるため

## 終了処理

- SIGINT / SIGTERM で graceful shutdown する (`docker stop` で末尾バッチが落ちない)
- graceful shutdown には 10 秒の猶予を設ける。body を宣言したまま送り切らない client 1 本でプロセスを止められなくなり、最後の `fsync` に到達できない事態を防ぐ。2 回目のシグナルで in-flight を即座に諦める
- proxy の queue に残った batch は破棄せず 5 秒間の drain で送り切る。再起動のたびに「上流がまだ受け取っていない分」を無言で失わないため
