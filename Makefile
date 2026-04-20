UI_DIR := $(CURDIR)/ui
SERVER_DIR := $(CURDIR)/server

.PHONY: help up up-no-build down logs dev watch test test-all test-vault

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*##"}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

up: ## Start the stack, rebuilding the server image if source changed
	docker compose up -d --build
	@$(MAKE) --no-print-directory test-db-migrate test-vault

up-no-build: ## Start the stack without rebuilding (use an existing image)
	docker compose up -d
	@$(MAKE) --no-print-directory test-db-migrate test-vault

test-db-migrate: ## Ensure schema is applied to the test Dolt (:3307)
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features mysql,rustls"; exit 1; }
	@until docker exec djinn-dolt-test dolt sql -q "SELECT 1" >/dev/null 2>&1; do sleep 1; done
	@cd $(SERVER_DIR)/crates/djinn-db && DATABASE_URL=mysql://root@127.0.0.1:3307/djinn sqlx migrate run --source migrations_mysql >/dev/null

test-vault: ## Create the test-only vault key at $DJINN_VAULT_KEY_PATH (idempotent)
	@mkdir -p /var/tmp/djinn-test-vault
	@if [ ! -f /var/tmp/djinn-test-vault/vault.key ]; then \
		openssl rand -out /var/tmp/djinn-test-vault/vault.key 32 && \
		chmod 600 /var/tmp/djinn-test-vault/vault.key && \
		echo "Created /var/tmp/djinn-test-vault/vault.key"; \
	fi

down: ## Stop the docker compose stack (volumes persist)
	docker compose down

logs: ## Tail djinn-server logs
	docker compose logs -f djinn-server

dev: ## Start the Vite web client (assumes `make up` was run)
	cd $(UI_DIR) && pnpm dev

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
	@$(MAKE) --no-print-directory test-db-migrate test-vault
	cd $(SERVER_DIR) && cargo test -p djinn-db

# Each crate's test suite spins up 100–250 fresh djinn_test_* databases and
# Dolt caches them all, so running the workspace concurrently saturates the
# 8 GiB test-Dolt and cascades into `UnexpectedEof` failures. This target
# runs each crate sequentially with a `test-db-reset` between them so the
# cache is drained before the next crate starts.
test-all: ## Run every workspace crate's tests sequentially (avoids test-Dolt OOM)
	@$(MAKE) --no-print-directory test-vault
	@$(MAKE) --no-print-directory test-db-reset
	cd $(SERVER_DIR) && cargo test -p djinn-db
	@$(MAKE) --no-print-directory test-db-reset
	cd $(SERVER_DIR) && cargo test -p djinn-core
	@$(MAKE) --no-print-directory test-db-reset
	cd $(SERVER_DIR) && cargo test -p djinn-provider
	@$(MAKE) --no-print-directory test-db-reset
	cd $(SERVER_DIR) && cargo test -p djinn-mcp
	@$(MAKE) --no-print-directory test-db-reset
	cd $(SERVER_DIR) && cargo test -p djinn-agent
	@$(MAKE) --no-print-directory test-db-reset
	cd $(SERVER_DIR) && cargo test -p djinn-server

# ----------------------------------------------------------------------------
# Kubernetes / Helm local-dev inner loop: see Tiltfile at the repo root.
# ----------------------------------------------------------------------------
# `tilt up` bootstraps the kind cluster + registry, builds images, installs
# the Helm release, and wires port-forwards. `kind delete cluster --name djinn`
# to tear down.
