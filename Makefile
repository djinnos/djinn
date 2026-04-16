DESKTOP_DIR := $(CURDIR)/desktop
SERVER_DIR := $(CURDIR)/server

.PHONY: help up up-no-build down logs dev watch

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*##"}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

up: ## Start the stack, rebuilding the server image if source changed
	docker compose up -d --build
	@$(MAKE) --no-print-directory test-db-migrate

up-no-build: ## Start the stack without rebuilding (use an existing image)
	docker compose up -d
	@$(MAKE) --no-print-directory test-db-migrate

test-db-migrate: ## Ensure schema is applied to the test Dolt (:3307)
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features mysql,rustls"; exit 1; }
	@until docker exec djinn-dolt-test dolt sql -q "SELECT 1" >/dev/null 2>&1; do sleep 1; done
	@cd $(SERVER_DIR)/crates/djinn-db && DATABASE_URL=mysql://root@127.0.0.1:3307/djinn sqlx migrate run --source migrations_mysql >/dev/null

down: ## Stop the docker compose stack (volumes persist)
	docker compose down

logs: ## Tail djinn-server logs
	docker compose logs -f djinn-server

dev: ## Start the Vite web client (assumes `make up` was run)
	cd $(DESKTOP_DIR) && pnpm dev

watch: ## Rebuild + restart djinn-server on Rust source changes (cargo-watch)
	@command -v cargo-watch >/dev/null 2>&1 || { echo "Install cargo-watch: cargo install cargo-watch"; exit 1; }
	cd $(SERVER_DIR) && cargo watch -w crates -w src -s 'cd $(CURDIR) && docker compose up -d --build djinn-server'

sqlx-prepare: ## Regenerate server/.sqlx/ offline cache (uses test Dolt on :3307 via .cargo/config.toml)
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features mysql,rustls"; exit 1; }
	@$(MAKE) --no-print-directory test-db-migrate
	cd $(SERVER_DIR) && cargo sqlx prepare --workspace

sqlx-check: ## Fail if server/.sqlx/ is stale vs. current queries (CI)
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features mysql,rustls"; exit 1; }
	@$(MAKE) --no-print-directory test-db-migrate
	cd $(SERVER_DIR) && cargo sqlx prepare --check --workspace

test-db-reset: ## Wipe and restart the test Dolt — cleans out djinn_test_* DBs
	docker compose stop dolt-test
	docker compose rm -sf dolt-test
	docker compose up -d dolt-test
	@$(MAKE) --no-print-directory test-db-migrate

test: ## Run djinn-db tests (env routes to :3307 via .cargo/config.toml)
	@$(MAKE) --no-print-directory test-db-migrate
	cd $(SERVER_DIR) && cargo test -p djinn-db
