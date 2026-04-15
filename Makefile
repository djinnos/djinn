DESKTOP_DIR := $(CURDIR)/desktop
SERVER_DIR := $(CURDIR)/server

.PHONY: help up up-no-build down logs dev watch

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*##"}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

up: ## Start the stack, rebuilding the server image if source changed
	docker compose up -d --build

up-no-build: ## Start the stack without rebuilding (use an existing image)
	docker compose up -d

down: ## Stop the docker compose stack (volumes persist)
	docker compose down

logs: ## Tail djinn-server logs
	docker compose logs -f djinn-server

dev: ## Start the Vite web client (assumes `make up` was run)
	cd $(DESKTOP_DIR) && pnpm dev

watch: ## Rebuild + restart djinn-server on Rust source changes (cargo-watch)
	@command -v cargo-watch >/dev/null 2>&1 || { echo "Install cargo-watch: cargo install cargo-watch"; exit 1; }
	cd $(SERVER_DIR) && cargo watch -w crates -w src -s 'cd $(CURDIR) && docker compose up -d --build djinn-server'

sqlx-prepare: ## Regenerate server/.sqlx/ offline cache (requires `make up` + sqlx-cli)
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features mysql,rustls"; exit 1; }
	cd $(SERVER_DIR) && DATABASE_URL=mysql://root@127.0.0.1:3306/djinn cargo sqlx prepare --workspace

sqlx-check: ## Fail if server/.sqlx/ is stale vs. current queries (CI)
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features mysql,rustls"; exit 1; }
	cd $(SERVER_DIR) && DATABASE_URL=mysql://root@127.0.0.1:3306/djinn cargo sqlx prepare --check --workspace
