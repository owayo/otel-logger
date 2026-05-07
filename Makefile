.PHONY: build release install init init-force run dry-run clean test fmt fmt-check check clippy \
        up up-d down restart logs stats compose-build compose-build-no-cache \
        docker docker-run help

.DEFAULT_GOAL := help

BINARY_NAME := otel-logger
INSTALL_PATH := /usr/local/bin
HTTP_ADDR := http://localhost:4318

# Cargo

build: ## Build debug version
	cargo build

release: ## Build release version
	cargo build --release

install: release ## Build release and install to /usr/local/bin
	cp target/release/$(BINARY_NAME) $(INSTALL_PATH)/

init: build ## Generate ~/.config/otel-logger/config.toml (refuses to overwrite)
	./target/debug/$(BINARY_NAME) init

init-force: build ## Same as init, but overwrite an existing file
	./target/debug/$(BINARY_NAME) init -f

run: ## Run the receiver locally with default ports
	cargo run -- --log-file ./otel-logger.jsonl

dry-run: ## Validate startup without binding ports
	cargo run -- --dry-run

# Development

test: ## Run tests
	cargo test

fmt: ## Format code
	cargo fmt

fmt-check: ## Check formatting
	cargo fmt -- --check

check: ## Run cargo check
	cargo check --all-targets

clippy: ## Run clippy
	cargo clippy --all-targets -- -D warnings

clean: ## Clean build artifacts
	cargo clean

# Docker Compose (default workflow)

up: ## Rebuild image and start otel-logger in foreground
	docker compose up --build otel-logger

up-d: ## Rebuild image and start otel-logger detached
	docker compose up -d --build otel-logger

down: ## Stop and remove the compose stack
	docker compose down

restart: ## Down + rebuild + up (full reset, foreground)
	docker compose down
	docker compose up --build otel-logger

logs: ## Tail otel-logger container logs
	docker compose logs -f otel-logger

stats: ## curl GET /stats from a running otel-logger container
	curl -s $(HTTP_ADDR)/stats | jq

compose-build: ## Build the image without starting (uses cache)
	docker compose build otel-logger

compose-build-no-cache: ## Rebuild from scratch (slow, ignores cache)
	docker compose build --no-cache otel-logger

# Docker (standalone, no compose)

docker: ## Build standalone Docker image
	docker build -t $(BINARY_NAME):dev .

docker-run: docker ## Build and run standalone container with mounted JSONL
	docker run --rm -p 4317:4317 -p 4318:4318 \
		-v $(CURDIR)/data:/var/log/otel-logger \
		$(BINARY_NAME):dev \
		--log-file /var/log/otel-logger/otel-logger.jsonl

# Help

help: ## Show this help message
	@echo "$(BINARY_NAME) Build Commands"
	@echo ""
	@echo "Usage: make [target]"
	@echo ""
	@echo "Targets:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "Common workflows:"
	@echo "  make init       # write ~/.config/otel-logger/config.toml"
	@echo "  make up         # rebuild + start (foreground)"
	@echo "  make up-d       # rebuild + start (detached)"
	@echo "  make logs       # tail logs"
	@echo "  make stats      # show cumulative usage stats"
	@echo "  make restart    # full reset (down + rebuild + up)"
	@echo ""
	@echo "Release:"
	@echo "  Use GitHub Actions > Release > Run workflow"
