# Development

Tasks beyond the standard make targets listed in the README. Tool versions are pinned in `mise.toml`; run `make setup` once before the commands below.

## Replaying saved telemetry

Replay saved JSONL back through the aggregator and print the resulting totals. This verifies aggregation against real telemetry without starting a server, and preserves arrival order so ordering-dependent double-counting bugs show up.

```bash
mise exec -- cargo run --release --locked --example replay_check -- otel-logger.jsonl
```

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
