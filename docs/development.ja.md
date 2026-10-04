# 開発

README に載せた標準の make ターゲット以外の作業です。ツールの版は `mise.toml` で固定しているので、先に一度 `make setup` を実行してください。

## 保存したテレメトリの再生

保存済み JSONL を集計器へ再生し、累計を出力します。サーバを起動せずに実テレメトリで集計を検証でき、到着順を保つため、順序依存の二重計上バグもそのまま再現します。

```bash
mise exec -- cargo run --release --locked --example replay_check -- otel-logger.jsonl
```

## 実ログによる検証 (2026-10-04)

Claude Code 2.1.288 と Codex 0.160.0 の成功した CI ログ 622 batch を到着順に再生し、decode 失敗が 0 件であることを確認しました。Claude の API request 26 件は token 4 種・cost が metrics と一致し、Codex の SSE 完了 190 件から tool-only 6 件を除いた合計も turn metrics の全 token 種別と一致しました。

ローカルの 2026-10-03 / 04 のログも、実行中ファイルの開始時点のバイト長までを固定して確認しました。usage と session 補完に必要な属性だけを残し、conversation ID を匿名化して再生した 13,635 / 8,159 batch は decode 失敗 0 件でした。独立した集計と累計が一致し、各 agent の total が bucket 合計と一致することも確認しています。進行中の logs と metrics は到着範囲が違うため、単純な全量比較を回帰判定には使いません。実ログや匿名化した再生ファイルはリポジトリへ含めません。

## 依存の監査

RustSec advisory database で `Cargo.lock` を検査します ([cargo-audit](https://crates.io/crates/cargo-audit) は `mise.toml` で固定していないので、別に入れてください)。

```bash
cargo audit
```

## Docker

コンテナイメージをビルドして動かす make のターゲットです。どれも `docker` を直接呼びます。

| コマンド | 説明 |
|---|---|
| `make docker` | 単体で動かす Docker イメージをビルドする |
| `make docker-run` | JSONL のディレクトリをマウントして単体のコンテナを起動する |
| `make up` / `make up-d` | イメージを作り直して compose のスタックを起動する (フォアグラウンド / バックグラウンド) |
| `make logs` | otel-logger のコンテナのログを追う |
| `make stats` | 起動中の otel-logger に `GET /stats` を送る |
| `make down` | compose のスタックを止めて削除する |
