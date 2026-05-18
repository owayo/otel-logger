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

- 受信した OTLP payload は JSON Lines で欠落なく保存する方針を守る。
- Claude の API request ログには token/cost が含まれ、metrics より新しい分まで即時に届くことがある。累計統計では同じ model/effort のログが見えたらログ側を token/cost source として採用し、先に計上した metrics 分は取り消して二重計上を避ける。
- Claude / Codex の累計統計は二重計上を避ける。特に Codex は SSE 完了ログと turn metrics の両方に token usage が出るため、model ごとに片方だけを token source として採用する。
- Codex の SSE 完了ログで `input_token_count == tool_token_count` かつ output/cache/reasoning がすべて 0 の tool-only event は、turn metrics / `handle_responses` span usage と合わせるため token usage に加算しない。
- Codex の SSE 完了ログは `conversation.id` で `codex.conversation_starts` の provider/model/effort に紐付ける。複数 conversation が混在するため、単純な「直近 session」だけで effort を決めない。`conversation.id` が付与されているが対応する session が未受信の場合は、別 conversation の `codex_last_session` にフォールバックせず、`effort=unknown` のまま処理する (後で session が届けば `merge_codex_unknown_effort` で正しいバケットへ統合される)。
- `log-file` / `log-dir` の相互排他は優先順位 (CLI/環境変数 > 設定ファイル > 既定値) を守る。上位で片方を指定した場合、下位のもう片方は衝突扱いにしない。
- `log-dir` cleanup は `otel-logger.YYYY-MM-DD` 形式の日次ローテーションファイルだけを削除する。`otel-logger.pid`、`otel-logger.stderr.log`、単体の `otel-logger.jsonl` など、同じ prefix の別用途ファイルは削除しない。
- コメントは、外部に出る CLI help では英日併記、内部実装の説明では日本語を基本にする。
