# Development tasks for otel-logger. Run `make` with no arguments to list the targets.
#
# Tool versions are pinned in mise.toml. When mise is available, every tool runs through
# `mise exec --`, so the pinned versions are used even when mise is not activated in the shell
# (for example when make is started from an IDE or a GUI). SYSTEM_TOOLS=1 uses the tools on PATH
# instead (the versions are then not guaranteed).
#
# The Docker targets (up, down, docker, ...) call docker directly; Docker is not managed by mise.
#
# Only GNU Make 3.81 features are used (the make that ships with macOS):
# no .ONESHELL, .SHELLFLAGS, $(file ...) or !=.

.DEFAULT_GOAL := help

BINARY_NAME := otel-logger
INSTALL_PATH ?= /usr/local/bin
# Cargo.lock is committed, so resolve dependencies exactly as CI does
CARGO_FLAGS ?= --locked
# Arguments for make run. The default writes JSON Lines next to the Makefile
ARGS ?= --log-file ./otel-logger.jsonl
# OTLP/HTTP listener that make stats queries
HTTP_ADDR := http://localhost:4318

# ---- Toolchain ------------------------------------------------------------------
# Look for mise on PATH, then in the usual install locations (make started from a GUI may not
# inherit the shell's PATH). Override with make MISE=/path/to/mise.
# To try the behavior without mise, empty the candidates with MISE_CANDIDATES=.
MISE_CANDIDATES ?= $(HOME)/.local/bin/mise /opt/homebrew/bin/mise /usr/local/bin/mise
ifeq ($(SYSTEM_TOOLS),1)
RUN :=
else
ifndef MISE
MISE := $(firstword $(shell command -v mise 2>/dev/null) $(wildcard $(MISE_CANDIDATES)))
endif
ifeq ($(MISE),)
ifneq ($(filter-out help,$(or $(MAKECMDGOALS),help)),)
$(error mise was not found. Install it from https://mise.jdx.dev, or add SYSTEM_TOOLS=1 to use the tools on PATH)
endif
endif
RUN := $(if $(MISE),$(MISE) exec --,)
endif

.PHONY: help setup build release run dry-run init init-force test lint clippy fmt fmt-check check ci \
        install uninstall clean up up-d down restart logs stats compose-build compose-build-no-cache \
        docker docker-run

## Setup

setup: ## Install the toolchain (mise) and dependencies
	@if [ -n "$(MISE)" ]; then "$(MISE)" install; fi
	$(RUN) cargo fetch $(CARGO_FLAGS)

## Build

build: ## Build a debug binary
	$(RUN) cargo build $(CARGO_FLAGS)

release: ## Build a release binary
	$(RUN) cargo build --release $(CARGO_FLAGS)

run: ## Run the debug binary (arguments via ARGS="..."). Default: --log-file ./otel-logger.jsonl
	$(RUN) cargo run $(CARGO_FLAGS) -- $(ARGS)

dry-run: ## Validate startup (config, listener bind) without serving
	$(RUN) cargo run $(CARGO_FLAGS) -- --dry-run

init: build ## Write ~/.config/otel-logger/config.toml (keeps an existing file)
	./target/debug/$(BINARY_NAME) init

init-force: build ## Same as init, but overwrites an existing file
	./target/debug/$(BINARY_NAME) init -f

## Checks

test: ## Run the tests
	$(RUN) cargo test $(CARGO_FLAGS)

lint: ## Run clippy with warnings as errors
	$(RUN) cargo clippy $(CARGO_FLAGS) --all-targets -- -D warnings

clippy: lint ## Alias of lint

fmt: ## Format the code (rewrites files)
	$(RUN) cargo fmt --all

fmt-check: ## Check the formatting (no changes)
	$(RUN) cargo fmt --all -- --check

check: fmt-check lint ## Run fmt-check and lint (no changes)

ci: check test ## Run the same checks as CI (no changes)

## Install

# Replace the binary through a temporary file and a rename instead of copying over it. macOS
# caches the code signature check per inode, so a binary copied over one that is running (or ran
# a moment ago) is killed with SIGKILL right after it starts (exit 137). The temporary file sits
# in the same directory so that the rename swaps the inode. The binary is not re-signed: the linker
# already signs it ad hoc, and a fixed identifier would not keep permissions across versions.
install: release ## Install the release binary to INSTALL_PATH (default /usr/local/bin)
	@mkdir -p "$(INSTALL_PATH)"
	cp "target/release/$(BINARY_NAME)" "$(INSTALL_PATH)/$(BINARY_NAME).new"
	mv -f "$(INSTALL_PATH)/$(BINARY_NAME).new" "$(INSTALL_PATH)/$(BINARY_NAME)"

uninstall: ## Remove the binary from INSTALL_PATH
	rm -f "$(INSTALL_PATH)/$(BINARY_NAME)"

clean: ## Remove build artifacts
	$(RUN) cargo clean

## Docker Compose (the default workflow for the sample stack)

up: ## Rebuild the image and start otel-logger in the foreground
	docker compose up --build otel-logger

up-d: ## Rebuild the image and start otel-logger in the background
	docker compose up -d --build otel-logger

down: ## Stop and remove the compose stack
	docker compose down

restart: ## Reset everything: down, rebuild and up (foreground)
	docker compose down
	docker compose up --build otel-logger

logs: ## Follow the otel-logger container log
	docker compose logs -f otel-logger

stats: ## Query GET /stats on the running otel-logger
	curl -s $(HTTP_ADDR)/stats | jq

compose-build: ## Build the image without starting it (uses the cache)
	docker compose build otel-logger

compose-build-no-cache: ## Rebuild the image from scratch without the cache (slow)
	docker compose build --no-cache otel-logger

## Docker (standalone, without compose)

docker: ## Build the standalone Docker image
	docker build -t $(BINARY_NAME):dev .

docker-run: docker ## Run the standalone container with the JSONL directory mounted
	docker run --rm -p 4317:4317 -p 4318:4318 \
		-v $(CURDIR)/data:/var/log/otel-logger \
		$(BINARY_NAME):dev \
		--log-file /var/log/otel-logger/otel-logger.jsonl

## Help

help: ## Show this help
	@echo "Development tasks for $(BINARY_NAME)"
	@echo ""
	@echo "Usage: make <target>"
	@echo ""
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "Common workflows:"
	@echo "  make init       # write ~/.config/otel-logger/config.toml"
	@echo "  make up         # rebuild and start the compose stack (foreground)"
	@echo "  make up-d       # rebuild and start the compose stack (background)"
	@echo "  make logs       # follow the container log"
	@echo "  make stats      # show the cumulative usage stats"
	@echo "  make restart    # full reset (down, rebuild, up)"
	@echo ""
	@echo "Tool versions are pinned in mise.toml. Run make setup first."
	@echo "Release: GitHub Actions > Release > Run workflow"
