UI_DIR := $(CURDIR)/ui
SERVER_DIR := $(CURDIR)/server

# Local-dev inner loop: `tilt up` at the repo root. It bootstraps the kind
# cluster + local registry, compiles djinn-server + djinn-agent-worker once
# (via scripts/tilt/build-binaries.sh), builds the agent-runtime base +
# thin images, installs the Helm release, and wires port-forwards. Nothing
# in this Makefile manages the dev stack anymore — only the isolated test
# MySQL (docker-compose.yml → `mysql-test` service at :3308) plus the test
# harness targets that depend on it.

.PHONY: help dev test-db-migrate test-db-mysql-template test-vault test-db-reset sqlx-prepare sqlx-check test test-all

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*##"}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

test-db-migrate: ## Ensure schema is applied to the test MySQL (:3308)
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features mysql,rustls"; exit 1; }
	@until docker exec djinn-mysql-test mysql -uroot -e "SELECT 1" >/dev/null 2>&1; do sleep 1; done
	@cd $(SERVER_DIR)/crates/djinn-db && DATABASE_URL=mysql://root@127.0.0.1:3308/djinn sqlx migrate run --source migrations_mysql >/dev/null

test-db-mysql-template: ## Build the djinn_test_template DB MySQL clones from
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features mysql,rustls"; exit 1; }
	@until docker exec djinn-mysql-test mysql -uroot -e "SELECT 1" >/dev/null 2>&1; do echo "waiting for mysql-test..."; sleep 1; done
	@docker exec djinn-mysql-test mysql -uroot -e "DROP DATABASE IF EXISTS djinn_test_template; CREATE DATABASE djinn_test_template"
	@cd $(SERVER_DIR)/crates/djinn-db && DATABASE_URL=mysql://root@127.0.0.1:3308/djinn_test_template sqlx migrate run --source migrations_mysql >/dev/null
	@echo "djinn_test_template ready"

test-vault: ## Create the test-only vault key at $DJINN_VAULT_KEY_PATH (idempotent)
	@mkdir -p /var/tmp/djinn-test-vault
	@if [ ! -f /var/tmp/djinn-test-vault/vault.key ]; then \
		openssl rand -out /var/tmp/djinn-test-vault/vault.key 32 && \
		chmod 600 /var/tmp/djinn-test-vault/vault.key && \
		echo "Created /var/tmp/djinn-test-vault/vault.key"; \
	fi

dev: ## Start the Vite web client standalone (Tilt also runs it — this is for UI-only sessions)
	cd $(UI_DIR) && pnpm dev

sqlx-prepare: ## Regenerate server/.sqlx/ offline cache (uses test MySQL on :3308 via .cargo/config.toml)
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features mysql,rustls"; exit 1; }
	@$(MAKE) --no-print-directory test-db-migrate
	@# Use `cargo check --all-targets --all-features` instead of `cargo sqlx prepare --workspace`:
	@# the latter (as of sqlx-cli 0.8.6) skips test targets, so queries inside
	@# `#[cfg(test)]` blocks silently miss the cache and break CI's offline build.
	@rm -rf /tmp/sqlx-prepare && mkdir -p /tmp/sqlx-prepare
	@# Force macro re-execution: touch every file with a sqlx::query call so
	@# cargo re-runs the proc-macro. A plain `cargo check` after a clean build
	@# would be a no-op and leave SQLX_OFFLINE_DIR empty.
	@grep -rl --include='*.rs' 'sqlx::query' $(SERVER_DIR)/crates/ 2>/dev/null | xargs -r touch
	@cd $(SERVER_DIR) && SQLX_OFFLINE_DIR=/tmp/sqlx-prepare cargo check --workspace --all-targets --all-features
	@if [ -z "$$(ls -A /tmp/sqlx-prepare 2>/dev/null)" ]; then \
		echo "ERROR: /tmp/sqlx-prepare is empty — refusing to replace .sqlx/."; \
		echo "       Try 'cargo clean -p djinn-db' and rerun."; \
		exit 1; \
	fi
	@# Replace only the query-*.json files; preserve README.md and anything
	@# else a human committed into .sqlx/.
	@find $(SERVER_DIR)/.sqlx -maxdepth 1 -name 'query-*.json' -delete
	@mv /tmp/sqlx-prepare/query-*.json $(SERVER_DIR)/.sqlx/
	@rm -rf /tmp/sqlx-prepare
	@echo "server/.sqlx/ regenerated ($$(ls $(SERVER_DIR)/.sqlx/query-*.json | wc -l) entries) — run 'git add server/.sqlx' and commit."

sqlx-check: ## Fail if server/.sqlx/ is stale vs. current queries (CI)
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features mysql,rustls"; exit 1; }
	@$(MAKE) --no-print-directory test-db-migrate
	cd $(SERVER_DIR) && cargo sqlx prepare --check --workspace

test-db-reset: ## Wipe and restart the test MySQL — cleans out djinn_test_* DBs
	docker compose stop mysql-test
	docker compose rm -sf mysql-test
	docker compose up -d mysql-test
	@$(MAKE) --no-print-directory test-db-migrate
	@$(MAKE) --no-print-directory test-db-mysql-template

test: ## Run djinn-db tests (env routes to :3308 via .cargo/config.toml)
	@$(MAKE) --no-print-directory test-db-migrate test-db-mysql-template test-vault
	cd $(SERVER_DIR) && cargo test -p djinn-db

# InnoDB + tmpfs handles the full workspace concurrently without the
# OOM cascade that Dolt's caching exhibited, so we can run the workspace
# in one shot via nextest.
test-all: ## Run every workspace crate's tests in one nextest invocation
	@$(MAKE) --no-print-directory test-vault
	@$(MAKE) --no-print-directory test-db-reset
	cd $(SERVER_DIR) && cargo nextest run --workspace
