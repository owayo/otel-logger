.PHONY: build release install init init-force run dry-run clean test fmt fmt-check check clippy \
        up up-d down restart logs stats compose-build compose-build-no-cache \
        docker docker-run help

.DEFAULT_GOAL := help

BINARY_NAME := otel-logger
INSTALL_PATH := /usr/local/bin
HTTP_ADDR := http://localhost:4318

# Cargo

build: ## debug build を作成する
	cargo build

release: ## release build を作成する
	cargo build --release

install: release ## release build を作成し /usr/local/bin へ install する
	cp target/release/$(BINARY_NAME) $(INSTALL_PATH)/
	@if command -v codesign >/dev/null 2>&1; then \
		codesign --force --sign - $(INSTALL_PATH)/$(BINARY_NAME); \
	fi

init: build ## ~/.config/otel-logger/config.toml を生成する (上書きしない)
	./target/debug/$(BINARY_NAME) init

init-force: build ## init と同じだが既存ファイルを上書きする
	./target/debug/$(BINARY_NAME) init -f

run: ## 既定 port で receiver をローカル実行する
	cargo run -- --log-file ./otel-logger.jsonl

dry-run: ## port を bind せず起動処理だけ検証する
	cargo run -- --dry-run

# 開発

test: ## test を実行する
	cargo test

fmt: ## code を format する
	cargo fmt

fmt-check: ## formatting を検証する
	cargo fmt -- --check

check: ## cargo check を実行する
	cargo check --all-targets

clippy: ## clippy を実行する
	cargo clippy --all-targets -- -D warnings

clean: ## build artifact を削除する
	cargo clean

# Docker Compose (既定 workflow)

up: ## image を rebuild し otel-logger を foreground で起動する
	docker compose up --build otel-logger

up-d: ## image を rebuild し otel-logger を detached で起動する
	docker compose up -d --build otel-logger

down: ## compose stack を停止して削除する
	docker compose down

restart: ## down + rebuild + up で全体を reset する (foreground)
	docker compose down
	docker compose up --build otel-logger

logs: ## otel-logger container log を tail する
	docker compose logs -f otel-logger

stats: ## 起動中の otel-logger container に GET /stats を送る
	curl -s $(HTTP_ADDR)/stats | jq

compose-build: ## 起動せず image だけ build する (cache 使用)
	docker compose build otel-logger

compose-build-no-cache: ## cache を使わず最初から rebuild する (遅い)
	docker compose build --no-cache otel-logger

# Docker (compose なしの standalone)

docker: ## standalone Docker image を build する
	docker build -t $(BINARY_NAME):dev .

docker-run: docker ## JSONL directory を mount して standalone container を起動する
	docker run --rm -p 4317:4317 -p 4318:4318 \
		-v $(CURDIR)/data:/var/log/otel-logger \
		$(BINARY_NAME):dev \
		--log-file /var/log/otel-logger/otel-logger.jsonl

# Help

help: ## この help message を表示する
	@echo "$(BINARY_NAME) build commands"
	@echo ""
	@echo "Usage: make [target]"
	@echo ""
	@echo "Targets:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "Common workflows:"
	@echo "  make init       # ~/.config/otel-logger/config.toml を書き出す"
	@echo "  make up         # rebuild + start (foreground)"
	@echo "  make up-d       # rebuild + start (detached)"
	@echo "  make logs       # log を tail する"
	@echo "  make stats      # 累計 usage stats を表示する"
	@echo "  make restart    # full reset (down + rebuild + up)"
	@echo ""
	@echo "Release:"
	@echo "  Use GitHub Actions > Release > Run workflow"
