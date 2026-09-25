# Integrations

Running `otel-logger` next to Claude Code or Codex in Docker Compose and in CI. How to point each agent at the receiver is in the README ([Sending telemetry from Claude Code](../README.md#sending-telemetry-from-claude-code), [Sending telemetry from OpenAI Codex CLI](../README.md#sending-telemetry-from-openai-codex-cli)).

## Docker Compose

The repo ships with a sample [`compose.yaml`](../compose.yaml) that runs `otel-logger` as a sidecar and shows two consumer containers (Claude Code and Codex):

```bash
docker compose up otel-logger
docker compose run --rm claude-code-sample
docker compose run --rm codex-sample
```

The receiver logs go to `docker compose logs otel-logger`, and the lossless JSONL stream lands in `./data/otel-logger.jsonl`.

The same stack is wrapped by make targets (`make up`, `make up-d`, `make logs`, `make stats`, `make down`); run `make` to list them.

## GitHub Actions

```yaml
jobs:
  ai-job:
    runs-on: ubuntu-latest
    services:
      otel-logger:
        image: ghcr.io/owayo/otel-logger:latest
        ports:
          - 4317:4317
          - 4318:4318
        options: >-
          --health-cmd "/usr/local/bin/otel-logger --dry-run --grpc-addr 127.0.0.1:0 --http-addr 127.0.0.1:0 --no-stdout"
          --health-interval 10s
          --health-timeout 3s
          --health-retries 3
    env:
      CLAUDE_CODE_ENABLE_TELEMETRY: "1"
      OTEL_LOGS_EXPORTER: otlp
      OTEL_METRICS_EXPORTER: otlp
      OTEL_TRACES_EXPORTER: otlp
      CLAUDE_CODE_ENHANCED_TELEMETRY_BETA: "1"
      OTEL_EXPORTER_OTLP_PROTOCOL: http/protobuf
      OTEL_EXPORTER_OTLP_ENDPOINT: http://localhost:4318
      OTEL_RESOURCE_ATTRIBUTES: "service.name=claude-code,deployment.environment=gha"
    steps:
      - uses: actions/checkout@v4
      - run: npm install -g @anthropic-ai/claude-code
      - run: claude --print "your prompt here"
```

## GitLab CI

```yaml
ai-job:
  image: node:20-bookworm-slim
  services:
    - name: ghcr.io/owayo/otel-logger:latest
      alias: otel-logger
  variables:
    CLAUDE_CODE_ENABLE_TELEMETRY: "1"
    OTEL_LOGS_EXPORTER: otlp
    OTEL_METRICS_EXPORTER: otlp
    OTEL_TRACES_EXPORTER: otlp
    CLAUDE_CODE_ENHANCED_TELEMETRY_BETA: "1"
    OTEL_EXPORTER_OTLP_PROTOCOL: http/protobuf
    OTEL_EXPORTER_OTLP_ENDPOINT: http://otel-logger:4318
    OTEL_RESOURCE_ATTRIBUTES: "service.name=claude-code,deployment.environment=gitlab"
  script:
    - npm install -g @anthropic-ai/claude-code
    - claude --print "your prompt here"
```
