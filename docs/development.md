# Development

Tasks beyond the standard make targets listed in the README. Tool versions are pinned in `mise.toml`; run `make setup` once before the commands below.

## Replaying saved telemetry

Replay saved JSONL back through the aggregator and print the resulting totals. This verifies aggregation against real telemetry without starting a server, and preserves arrival order so ordering-dependent double-counting bugs show up.

```bash
mise exec -- cargo run --release --locked --example replay_check -- otel-logger.jsonl
```

Decode failures are reported on stderr and produce a nonzero exit status. Totals from valid batches are still printed for diagnosis; a successful verification requires zero failures. Read errors also return a nonzero status.

## Real-log verification (2026-11-12)

Frozen local captures from November 11 and 12 contained 6,971 and 10,254 complete batches. Replaying every batch in arrival order produced zero decode failures. Independently calculated token, cost, request count, and duration totals matched the aggregator; each agent's total also equaled the sum of all its buckets. This verification did not include a new CI artifact.

The captures included Claude Code 2.1.322, Codex 0.167.0–0.167.2, and Desktop server builds 0.167.0-alpha.3.2 / alpha.4.1. Of 312 and 759 Codex SSE completions, 37 and 25 tool-only events were excluded according to the existing rule. Active logs and metrics cover different arrival ranges, so comparing their entire raw totals is not a regression criterion.

Usage extraction continues to use Claude's aggregate `cache_creation_tokens`; additional cache-duration and agent attributes do not add usage a second time. Regression cases cover the non-usage events `claude_code.compaction`, `claude_code.subagent_completed`, and `codex.sandbox_outcome`. No new aggregation branch was needed. Raw logs and replay files are kept outside the repository.

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
