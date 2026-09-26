# otel-logger の開発タスク。引数なしの `make` でターゲット一覧を表示する。
#
# ツールのバージョンは mise.toml に固定する。mise が使える場合、IDE や GUI から
# 起動して shell で有効化されていなくても `mise exec --` 経由で固定版を使う。
# SYSTEM_TOOLS=1 では PATH 上のツールを使うため、バージョンは保証されない。
#
# Docker 関連のターゲットは docker を直接呼び、mise では管理しない。
#
# macOS 付属の GNU Make 3.81 で使える機能に限る。
# .ONESHELL、.SHELLFLAGS、$(file ...) と != は使わない。

.DEFAULT_GOAL := help

BINARY_NAME := otel-logger
INSTALL_PATH ?= /usr/local/bin
# Cargo.lock をコミットし、CI と同じ依存を解決する。
CARGO_FLAGS ?= --locked
# make run の引数。既定では Makefile の隣へ JSON Lines を書く。
ARGS ?= --log-file ./otel-logger.jsonl
# make stats が問い合わせる OTLP/HTTP の受信先。
HTTP_ADDR := http://localhost:4318

# ---- ツールチェーン --------------------------------------------------------------
# mise を PATH と通常のインストール先から探す。GUI 経由では shell の PATH を
# 継承しない場合がある。make MISE=/path/to/mise で指定できる。
# mise なしの動作確認では MISE_CANDIDATES= で候補を空にする。
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

## 準備

setup: ## Install the toolchain (mise) and dependencies
	@if [ -n "$(MISE)" ]; then "$(MISE)" install; fi
	$(RUN) cargo fetch $(CARGO_FLAGS)

## ビルド

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

## 検証

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

## インストール

# バイナリは直接上書きせず、同じディレクトリの一時ファイルから rename で置き換える。
# macOS は署名検証を inode ごとにキャッシュするため、直前まで動いていた実行ファイルを
# 上書きすると起動直後に SIGKILL される場合がある (終了コード 137)。
# リンカが ad-hoc 署名を付けるため再署名はしない。固定 identifier を付けても
# バージョン間で権限は引き継がれない。
install: release ## Install the release binary to INSTALL_PATH (default /usr/local/bin)
	@mkdir -p "$(INSTALL_PATH)"
	cp "target/release/$(BINARY_NAME)" "$(INSTALL_PATH)/$(BINARY_NAME).new"
	mv -f "$(INSTALL_PATH)/$(BINARY_NAME).new" "$(INSTALL_PATH)/$(BINARY_NAME)"

uninstall: ## Remove the binary from INSTALL_PATH
	rm -f "$(INSTALL_PATH)/$(BINARY_NAME)"

clean: ## Remove build artifacts
	$(RUN) cargo clean

## Docker Compose (サンプル構成の既定の運用手順)

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

## Docker (Compose を使わない単体起動)

docker: ## Build the standalone Docker image
	docker build -t $(BINARY_NAME):dev .

docker-run: docker ## Run the standalone container with the JSONL directory mounted
	docker run --rm -p 4317:4317 -p 4318:4318 \
		-v $(CURDIR)/data:/var/log/otel-logger \
		$(BINARY_NAME):dev \
		--log-file /var/log/otel-logger/otel-logger.jsonl

## ヘルプ

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
