# 開発

README に載せた標準の make ターゲット以外の作業です。ツールの版は `mise.toml` で固定しているので、先に一度 `make setup` を実行してください。

## 保存したテレメトリの再生

保存済み JSONL を集計器へ再生し、累計を出力します。サーバを起動せずに実テレメトリで集計を検証でき、到着順を保つため、順序依存の二重計上バグもそのまま再現します。

```bash
mise exec -- cargo run --release --locked --example replay_check -- otel-logger.jsonl
```

decode 失敗は stderr へ報告し、終了コードを非 0 にします。有効な batch の累計は診断用に出力しますが、検証成功には失敗 0 件が必要です。読み取りエラーでも非 0 で終了します。

## 実ログによる検証 (2026-11-12)

ローカルの11月11日・12日のログを固定し、完全な行である6,971 / 10,254 batchを全件、到着順に再生しました。decode 失敗は0件でした。独立した計算とtoken・cost・request count・durationの累計が一致し、各agentのtotalも全bucketの合計と一致しました。今回の検証には新しいCIアーティファクトを含めていません。

ログには Claude Code 2.1.322、Codex 0.167.0〜0.167.2、Desktop server の 0.167.0-alpha.3.2 / alpha.4.1 が含まれていました。Codex の SSE 完了312 / 759件のうち、既存の規則でtool-onlyの37 / 25件を除外しています。進行中のlogsとmetricsは到着範囲が違うため、単純な全量比較を回帰判定には使いません。

Claude のキャッシュ作成tokenは引き続き合計の `cache_creation_tokens` を使い、有効期間別やagent情報の追加属性からは重複加算しません。使用量を持たない `claude_code.compaction`、`claude_code.subagent_completed`、`codex.sandbox_outcome` は回帰テストの対象に加えました。集計の分岐を追加する必要はありませんでした。実ログや再生ファイルはリポジトリへ含めません。

## 依存の監査

RustSec advisory database で `Cargo.lock` を検査します。[cargo-audit](https://crates.io/crates/cargo-audit) は `mise.toml` で版を固定し、`make setup` でインストールします。

```bash
mise exec -- cargo audit
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
