# Development

Tasks beyond the standard make targets listed in the README. Tool versions are pinned in `mise.toml`; run `make setup` once before the commands below.

## Replaying saved telemetry

Replay saved JSONL back through the aggregator and print the resulting totals. This verifies aggregation against real telemetry without starting a server, and preserves arrival order so ordering-dependent double-counting bugs show up.

```bash
mise exec -- cargo run --release --locked --example replay_check -- otel-logger.jsonl
```

Decode failures are reported on stderr and produce a nonzero exit status. Totals from valid batches are still printed for diagnosis; a successful verification requires zero failures. Read errors also return a nonzero status.

## Real-log verification (2026-10-08)

A successful CI capture from Claude Code 2.1.292 and Codex 0.160.1 replayed all 180 batches in arrival order with zero decode failures. Claude's 14 API requests matched metrics for all four token types and cost. Codex's 34 SSE completions, excluding two tool-only events, matched turn metrics for every token type.

Local captures from October 7 and a frozen October 8 snapshot contained 23,467 and 2,584 batches. Retaining usage and session metadata and hashing conversation IDs yielded 8,725 and 1,314 replay batches, both with zero decode failures. Independent totals matched the aggregator, and each agent's total equaled the sum of its buckets. Five Desktop conversations used `Codex Desktop`; none shared a conversation or completion with another service. Exec Server emitted operational traces without usage. Active logs and metrics cover different arrival ranges, so comparing their entire raw totals is not a regression criterion. Raw logs and sanitized replay files are kept outside the repository.

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
