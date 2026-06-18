# AGENTS.md

このリポジトリで作業する agent 向けのメモです。

## 基本

- 回答は日本語で行う。
- Rust 2024 edition / MSRV 1.88 の CLI + library project として扱う。
- 変更前に `astro-sight context --dir . --git`、変更後に `astro-sight impact --dir . --git` を実行する。
- code identifier の利用箇所を探す場合は `grep` / `rg` ではなく `astro-sight refs` を使う。
- `CLAUDE.md` は `AGENTS.md` への symlink なので直接編集しない。

## よく使う検証

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

`make install` は release build 後に `/usr/local/bin/otel-logger` へ配置する。macOS では配置後に ad-hoc 署名する。

## 実装上の注意

- 受信した OTLP payload は JSON Lines で欠落なく保存する方針を守る。JSONL 書き込みに失敗した場合は HTTP 5xx / gRPC `Status::internal` を返し、OTLP exporter 側に retry させる (`sink::Sink::record` の戻り値経由)。累計使用量は JSONL 永続化に成功した batch だけを集計し、retry 対象を二重計上しない。stdout への pretty 出力はベストエフォートで、失敗してもログだけ残して継続する。
- JSONL `write_line` は batch ごとに `BufWriter::flush` を呼び、ACK 時点で kernel に書き渡しておく。`sync_all` まで毎回呼ぶと throughput が落ちるため、disk 確定は graceful shutdown 時の `flush()` (= file の場合 `sync_all`) に委ねる。これで小さい batch がメモリバッファに留まったまま process crash で消える事故を防ぐ。
- Claude の API request ログには token/cost が含まれ、metrics より新しい分まで即時に届くことがある。累計統計では同じ model/effort のログが見えたらログ側を token/cost source として採用し、先に計上した metrics 分は取り消して二重計上を避ける。
- Claude / Codex の累計統計は二重計上を避ける。特に Codex は SSE 完了ログと turn metrics の両方に token usage が出るため、model ごとに片方だけを token source として採用する。
- Codex の SSE 完了ログで `input_token_count == tool_token_count` かつ output/cache/reasoning がすべて 0 の tool-only event は、turn metrics / `handle_responses` span usage と合わせるため token usage に加算しない。
- Codex 0.140.0 以降の `codex.websocket_event` の `response.completed` は、実ログ上 token usage を持たず、対応する usage は `codex.sse_event` / `codex.turn.token_usage` / `session_task.turn` span 側に出る。usage を持たない websocket completion で token source を Logs に切り替えない。`session_task.turn` span の `codex.turn.token_usage.*` は metric / SSE と同じ usage の別表現なので、trace span からは二重計上しない。
- Codex の SSE 完了ログは `conversation.id` で `codex.conversation_starts` の provider/model/effort に紐付ける。複数 conversation が混在するため、単純な「直近 session」だけで effort を決めない。`conversation.id` が付与されているが対応する session が未受信の場合は、別 conversation の `codex_last_session` にフォールバックせず、`effort=unknown` のまま処理する (後で session が届けば `merge_codex_unknown_effort` で正しいバケットへ統合される)。
- `update_codex_effort_from_request_attrs` (`handle_responses` span 経由の effort 補完) も `conversation.id` 単位で動かす。span に `conversation.id` がある場合は対応する `codex_sessions` の effort だけを更新し、別 conversation の `codex_last_session` を巻き込まない。`conversation.id` が無い古い span のみ、最後の session に対する fallback として動作させる。
- `merge_codex_unknown_effort` は `conversation.id` 単位で pending 分だけを移動する。並行する別 conversation の token を巻き込まないよう、`codex_unknown_effort_sse` に conversation 別の累計を保留しておき、対応する session が届いた conversation の分だけ unknown バケットから新 effort バケットへ振り替える。conversation.id が無い古い telemetry に限り、unknown バケット全体を移す旧挙動を残す (ただし他 conversation の pending 合計は差し引いて取り違えを防ぐ)。
- `log-file` / `log-dir` の相互排他は優先順位 (CLI/環境変数 > 設定ファイル > 既定値) を守る。上位で片方を指定した場合、下位のもう片方は衝突扱いにしない。
- `--config`、`otel-logger init --path`、`log-file`、`log-dir` は先頭の `~` / `~/` を `$HOME` に展開する。shell が展開しない環境変数・設定ファイル由来の値も同じ扱いにする。`HOME` が空文字 (`HOME=""`、cron / systemd / コンテナで実在する) の場合は未設定と同等に扱い、`~/x` を CWD 相対パスへ化けさせない (`default_config_path` の空 `HOME` 弾きと挙動を揃える)。
- `log-dir` cleanup は `otel-logger.YYYY-MM-DD` 形式の日次ローテーションファイルだけを削除する。`otel-logger.pid`、`otel-logger.stderr.log`、単体の `otel-logger.jsonl` など、同じ prefix の別用途ファイルは削除しない。
- `otel-logger init` の上書き拒否は `OpenOptions::create_new` で atomic に行い、`exists()` 後の TOCTOU race で `--force` 未指定の既存ファイルを壊さない。
- pretty stdout 出力 (`format::quote_for_pretty`) では ANSI escape を含む C0/C1 制御文字を必ず escape する。body/属性値だけでなく service 名、scope 名、span 名、metric 名、severity text、属性キー、累計サマリー内の provider/model/effort も対象にする。OTLP は既定で `0.0.0.0` に bind するため、信頼できない telemetry source からの terminal escape injection を防ぐ。JSONL 出力は lossless のまま raw 値を保持する。
- コメントは、外部に出る CLI help では英日併記、内部実装の説明では日本語を基本にする。
- `int_attr` / `f64_attr` / `number_value_as_u64` / `histogram_sum_as_u64` は `DoubleValue` / histogram `sum` を受け取る際に必ず有限性と範囲を検査する。OTLP は既定で `0.0.0.0` に bind され、信頼できない source から NaN / Infinity / range 外の double が届く可能性があるため、サチった `i64::MAX` / `u64::MAX` や `cost_usd=inf` で累計が破壊されないよう、有限かつ範囲内の値だけを採用する。
- `ModelStats` の累計加算は必ず saturating にする。個々の値を検証しても、複数 batch の合算で `u64` overflow や `cost_usd=inf` が起こると `/stats` と summary が壊れるため、外部入力由来の counter は wrap / panic させない。
- OTLP/gRPC・OTLP/HTTP の 1 リクエスト上限は `OTLP_MAX_REQUEST_BYTES` (32MiB) に引き上げる。tonic 既定 4MiB / axum 既定 2MiB のままだと大きな batch が `RESOURCE_EXHAUSTED` / `413 Payload Too Large` で恒久的に拒否され、exporter が retry しても同じ size のため回復できず「受信した payload を欠落なく保存」方針が崩れる。`serve_grpc` では生成した各 service server に `max_decoding_message_size`、`http::router` には `DefaultBodyLimit::max` を設定する。実測の最大 batch (約 0.6MiB) に十分な余裕を取りつつ、`0.0.0.0` 公開 bind 時のメモリ枯渇を避けるため 32MiB を上限とする。
- OTLP/HTTP の `Content-Type` 判定 (`http::detect_encoding`) は media type (type/subtype) を大小文字非区別で比較する (RFC 9110)。`Application/X-Protobuf; charset=utf-8` のような大文字混じり・パラメータ付きの表記でも `application/x-protobuf` / `application/json` として受け付け、decode 前に `415 Unsupported Media Type` で正当な OTLP request を弾かない。`;` 以降のパラメータを除いた primary type を `eq_ignore_ascii_case` で判定する。
- `update_codex_effort_from_request_attrs` (`handle_responses` span 経由の effort 補完) は、`conversation.id` を持つ span では当該 conversation の `codex_sessions` だけを更新し、`codex_last_session` を巻き込まない (`update_codex_session_with_last(.., false)`)。`codex_last_session` を上書きすると、`conversation.id` を持たない後続 metric (`codex.conversation.turn.count` / `codex.turn.e2e_duration_ms` など) の effort バケットが、無関係な別 conversation の span に引きずられる。`conversation.id` が無い古い span、または last session と同じ conversation の場合だけ last を更新する。**例外: `codex_last_session` が `None` の初回 seed では、`conversation.id` 付き span でも last を設定する。上書き対象の別 conversation が存在しないため方針には反さず、また seed しないと後続 metric が `effort=unknown` バケットへ落ちて span から拾えた effort 情報が失われる (回帰テスト: `codex_handle_responses_span_with_conversation_id_seeds_empty_last_session`)。
- Codex 系の `service.name` 判定は `is_codex_service` ヘルパーへ集約する。現状の対象は TUI (`codex_cli_rs`)、Exec (`codex_exec`)、Apps Server (`codex-app-server`) の 3 種。Codex 0.140.0 以降の Apps Server (`codex_apps` 経由) は metrics を送らず logs (`codex.conversation_starts` / `codex.sse_event`) と一部 trace span のみを送ってくるため、これを集計対象から外すと Apps Server 経由の token usage が累計から欠落する。新しい Codex バイナリが増えた場合は `aggregator::is_codex_service` に service.name 定数を追加すれば logs/traces/metrics すべての経路に反映される。
