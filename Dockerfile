# syntax=docker/dockerfile:1.7

# ---------- chef ----------
# cargo-chef caches dependency compilation between builds. Crucial because
# tonic / opentelemetry-proto take several minutes to compile from scratch.
FROM rust:1.90-slim-bookworm AS chef
WORKDIR /app
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config \
 && rm -rf /var/lib/apt/lists/* \
 && cargo install cargo-chef --locked --version ^0.1

# ---------- planner ----------
FROM chef AS planner
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

# ---------- builder ----------
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
RUN cargo build --release --bin otel-logger \
 && strip target/release/otel-logger

# ---------- runtime ----------
# distroless/cc-debian12 ships glibc + libgcc + libssl just in case. nonroot
# variant runs as uid 65532 by default.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=builder /app/target/release/otel-logger /usr/local/bin/otel-logger
EXPOSE 4317 4318
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/otel-logger"]
CMD ["--grpc-addr", "0.0.0.0:4317", "--http-addr", "0.0.0.0:4318"]
