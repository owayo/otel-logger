# 開発

README に載せた標準の make ターゲット以外の作業です。ツールの版は `mise.toml` で固定しているので、先に一度 `make setup` を実行してください。

## 保存したテレメトリの再生

保存済み JSONL を集計器へ再生し、累計を出力します。サーバを起動せずに実テレメトリで集計を検証でき、到着順を保つため、順序依存の二重計上バグもそのまま再現します。

```bash
mise exec -- cargo run --release --locked --example replay_check -- otel-logger.jsonl
```

decode 失敗は stderr へ報告し、終了コードを非 0 にします。有効な batch の累計は診断用に出力しますが、検証成功には失敗 0 件が必要です。読み取りエラーでも非 0 で終了します。

## 実ログによる検証 (2026-10-22)

Claude Code 2.1.306 と Codex 0.163.0 の成功した CI ログ 161 batch を到着順に再生し、decode 失敗が 0 件であることを確認しました。Claude の API request 12 件は token 4 種・cost が metrics と一致し、Codex の SSE 完了 38 件から tool-only 2 件を除いた合計も turn metrics の全 token 種別と一致しました。

ローカルの 2026-10-21 と固定した 10-22 のログは 25,191 / 17,861 batch でした。全量を走査したうえで、usage と session 補完に必要な属性を残した 10,861 / 7,824 batch を到着順に再生し、decode 失敗 0 件を確認しました。独立した集計と token・cost・request count・duration の累計が一致し、各 agent の total が bucket 合計と一致することも確認しています。

ローカルでは Claude Code 2.1.304〜2.1.308、Codex 0.163.0、Desktop 同梱の 0.163.0-alpha.10.2 / alpha.11.1 を確認しました。新しいモデル名も既存の集計で扱えています。`node_repl` の `codex.browser_use.authority_acquisition` は使用量を持たない運用イベントでした。進行中の logs と metrics は到着範囲が違うため、単純な全量比較を回帰判定には使いません。実ログや加工した再生ファイルはリポジトリへ含めません。

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
