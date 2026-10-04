# Development

Tasks beyond the standard make targets listed in the README. Tool versions are pinned in `mise.toml`; run `make setup` once before the commands below.

## Replaying saved telemetry

Replay saved JSONL back through the aggregator and print the resulting totals. This verifies aggregation against real telemetry without starting a server, and preserves arrival order so ordering-dependent double-counting bugs show up.

```bash
mise exec -- cargo run --release --locked --example replay_check -- otel-logger.jsonl
```

## Real-log verification (2026-10-04)

A successful CI capture from Claude Code 2.1.288 and Codex 0.160.0 replayed all 622 batches in arrival order with zero decode failures. Claude's 26 API requests matched metrics for all four token types and cost. Codex's 190 SSE completions, excluding six tool-only events, matched turn metrics for every token type.

Local captures from October 3 and 4 were frozen at their initial byte length before analysis. Retaining only usage and session metadata and anonymizing conversation IDs yielded 13,635 and 8,159 replay batches, both with zero decode failures. Independent token totals matched the aggregator, and each agent's total equaled the sum of its buckets. Active logs and metrics cover different arrival ranges, so comparing their entire raw totals is not a regression criterion. Raw logs and sanitized replay files are kept outside the repository.

## Dependency audit

Scan `Cargo.lock` with the RustSec advisory database ([cargo-audit](https://crates.io/crates/cargo-audit) is not pinned in `mise.toml`; install it separately).

```bash
cargo audit
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
