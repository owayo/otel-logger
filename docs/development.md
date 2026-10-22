# Development

Tasks beyond the standard make targets listed in the README. Tool versions are pinned in `mise.toml`; run `make setup` once before the commands below.

## Replaying saved telemetry

Replay saved JSONL back through the aggregator and print the resulting totals. This verifies aggregation against real telemetry without starting a server, and preserves arrival order so ordering-dependent double-counting bugs show up.

```bash
mise exec -- cargo run --release --locked --example replay_check -- otel-logger.jsonl
```

Decode failures are reported on stderr and produce a nonzero exit status. Totals from valid batches are still printed for diagnosis; a successful verification requires zero failures. Read errors also return a nonzero status.

## Real-log verification (2026-10-22)

A successful CI capture from Claude Code 2.1.306 and Codex 0.163.0 replayed all 161 batches in arrival order with zero decode failures. Claude's 12 API requests matched metrics for all four token types and cost. Codex's 38 SSE completions, excluding two tool-only events, matched turn metrics for every token type.

Local captures from October 21 and a frozen October 22 snapshot contained 25,191 and 17,861 batches. After scanning the complete captures, retaining usage and session metadata yielded 10,861 and 7,824 replay batches, both with zero decode failures. Independent token, cost, request count, and duration totals matched the aggregator, and each agent's total equaled the sum of its buckets.

The local captures included Claude Code 2.1.304–2.1.308, Codex 0.163.0, and Desktop builds 0.163.0-alpha.10.2 / alpha.11.1. Existing aggregation handled new model names. The `node_repl` event `codex.browser_use.authority_acquisition` carried operational metadata without usage. Active logs and metrics cover different arrival ranges, so comparing their entire raw totals is not a regression criterion. Raw logs and processed replay files are kept outside the repository.

## Dependency audit

Scan `Cargo.lock` with the RustSec advisory database. [cargo-audit](https://crates.io/crates/cargo-audit) is pinned in `mise.toml` and installed by `make setup`.

```bash
mise exec -- cargo audit
```

## Docker

The make targets below build and run the container image; they call `docker` directly.

| Command | Description |
|---|---|
| `make docker` | Build the standalone Docker image |
| `make docker-run` | Run the standalone container with the JSONL directory mounted |
| `make up` / `make up-d` | Rebuild the image and start the compose stack (foreground / background) |
| `make logs` | Follow the otel-logger container log |
| `make stats` | Query `GET /stats` on the running otel-logger |
| `make down` | Stop and remove the compose stack |
