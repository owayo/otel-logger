# 連携

Docker Compose や CI で、Claude Code / Codex の隣に `otel-logger` を置いて動かす例です。各エージェントの送信先を受信サーバに向ける設定は README にあります ([Claude Code からの送信](../README.ja.md#claude-code-からの送信)、[OpenAI Codex CLI からの送信](../README.ja.md#openai-codex-cli-からの送信))。

## Docker Compose

`otel-logger` をサイドカーとして、Claude Code / Codex を別コンテナで動かす [`compose.yaml`](../compose.yaml) を同梱しています。

```bash
docker compose up otel-logger
docker compose run --rm claude-code-sample
docker compose run --rm codex-sample
```

サーバ側のログは `docker compose logs otel-logger`、JSONL は `./data/otel-logger.jsonl` に出力されます。

同じ操作は make のターゲット (`make up`、`make up-d`、`make logs`、`make stats`、`make down`) にもまとめてあります。一覧は `make` で表示できます。

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
