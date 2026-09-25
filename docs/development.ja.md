# 開発

README に載せた標準の make ターゲット以外の作業です。ツールの版は `mise.toml` で固定しているので、先に一度 `make setup` を実行してください。

## 保存したテレメトリの再生

保存済み JSONL を集計器へ再生し、累計を出力します。サーバを起動せずに実テレメトリで集計を検証でき、到着順を保つため、順序依存の二重計上バグもそのまま再現します。

```bash
mise exec -- cargo run --release --locked --example replay_check -- otel-logger.jsonl
```

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
