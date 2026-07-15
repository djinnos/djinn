UI_DIR := $(CURDIR)/ui
SERVER_DIR := $(CURDIR)/server

# Local-dev inner loop: `tilt up` at the repo root. It bootstraps the kind
# cluster + local registry, compiles djinn-server + djinn-agent-worker once
# (via scripts/tilt/build-binaries.sh), builds the agent-runtime base +
# thin images, installs the Helm release, and wires port-forwards. Nothing
# in this Makefile manages the dev stack anymore — only the isolated test
# Postgres (docker-compose.yml → `postgres-test` service at :5433) plus the
# test harness targets that depend on it.

.PHONY: help dev test-db-migrate test-db-postgres-template test-vault test-db-reset sqlx-prepare sqlx-check sqlx-verify skills-manifest-generate skills-manifest-check test test-all validate-taskrun-backstop check-boundaries verify-cache-cleanup

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*##"}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

verify-cache-cleanup: ## Run the read-only cache-cleanup acceptance verifier
	@./scripts/verify-cache-cleanup.sh

test-db-migrate: ## Ensure schema is applied to the test Postgres (:5433)
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features postgres,rustls"; exit 1; }
	@until docker exec djinn-postgres-test pg_isready -U postgres >/dev/null 2>&1; do sleep 1; done
	@cd $(SERVER_DIR)/crates/djinn-db && DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5433/djinn sqlx migrate run --source migrations_postgres >/dev/null

test-db-postgres-template: ## Build the djinn_test_template DB Postgres clones from
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features postgres,rustls"; exit 1; }
	@until docker exec djinn-postgres-test pg_isready -U postgres >/dev/null 2>&1; do echo "waiting for postgres-test..."; sleep 1; done
	@# Evict any sessions still attached to the template before dropping; Postgres
	@# refuses DROP DATABASE while connections remain.
	@docker exec djinn-postgres-test psql -U postgres -d postgres -v ON_ERROR_STOP=1 -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='djinn_test_template' AND pid <> pg_backend_pid()" >/dev/null
	@docker exec djinn-postgres-test psql -U postgres -d postgres -v ON_ERROR_STOP=1 -c "DROP DATABASE IF EXISTS djinn_test_template" >/dev/null
	@docker exec djinn-postgres-test psql -U postgres -d postgres -v ON_ERROR_STOP=1 -c "CREATE DATABASE djinn_test_template" >/dev/null
	@cd $(SERVER_DIR)/crates/djinn-db && DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5433/djinn_test_template sqlx migrate run --source migrations_postgres >/dev/null
	@# Mark the template as a TEMPLATE so it can be used as a fast clone source
	@# (CREATE DATABASE x TEMPLATE djinn_test_template). Required for the test-
	@# harness clone path in Database::open_in_memory().
	@docker exec djinn-postgres-test psql -U postgres -d postgres -v ON_ERROR_STOP=1 -c "UPDATE pg_database SET datistemplate = TRUE WHERE datname = 'djinn_test_template'" >/dev/null
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

sqlx-prepare: ## Regenerate server/.sqlx/ offline cache (uses test Postgres on :5433 via .cargo/config.toml)
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features postgres,rustls"; exit 1; }
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

sqlx-check: ## Fail if server/.sqlx/ is stale vs. current queries (local)
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features postgres,rustls"; exit 1; }
	@# Local convenience: bring the docker test Postgres up to schema, then
	@# run the DB-agnostic verifier. CI applies migrations with its own step
	@# (service container, no `docker exec`) and calls `sqlx-verify` directly.
	@$(MAKE) --no-print-directory test-db-migrate
	@$(MAKE) --no-print-directory sqlx-verify

sqlx-verify: ## Verify server/.sqlx/ freshness; assumes schema already applied + DATABASE_URL reachable (CI)
	@command -v sqlx >/dev/null 2>&1 || { echo "Install sqlx-cli: cargo install sqlx-cli --no-default-features --features postgres,rustls"; exit 1; }
	@# Regenerate into a scratch dir using the EXACT same surface as
	@# `sqlx-prepare` (--all-targets --all-features) and force macro
	@# re-execution by touching every sqlx::query file, then diff against the
	@# committed cache. `cargo sqlx prepare --check` is NOT sufficient: it
	@# skips test targets, ignores feature-gated queries, and — because the
	@# sqlx proc-macro only reads/writes query data when its source file is
	@# (re)compiled — a warm build cache lets a stale/deleted .sqlx entry pass
	@# undetected. That exact gap silently dropped 71 `#[cfg(test)]` entries
	@# (commit ed6f954d5) which then only failed in the push-only Warm Cache
	@# (Test) job. Mirroring the generator here closes it.
	@rm -rf /tmp/sqlx-check && mkdir -p /tmp/sqlx-check
	@grep -rl --include='*.rs' 'sqlx::query' $(SERVER_DIR)/crates/ 2>/dev/null | xargs -r touch
	@cd $(SERVER_DIR) && SQLX_OFFLINE_DIR=/tmp/sqlx-check cargo check --workspace --all-targets --all-features
	@if [ -z "$$(ls -A /tmp/sqlx-check 2>/dev/null)" ]; then \
		echo "ERROR: regeneration produced no query files — cannot validate .sqlx/."; \
		echo "       Try 'cargo clean -p djinn-db' and rerun."; \
		exit 1; \
	fi
	@# The freshly generated set must match the committed cache exactly
	@# (same query hashes AND same type info). Any difference => stale cache.
	@stale=0; \
	for f in /tmp/sqlx-check/query-*.json; do \
		b=$$(basename $$f); \
		if [ ! -f "$(SERVER_DIR)/.sqlx/$$b" ]; then \
			echo "::error::missing committed .sqlx entry: $$b"; stale=1; \
		elif ! cmp -s "$$f" "$(SERVER_DIR)/.sqlx/$$b"; then \
			echo "::error::out-of-date committed .sqlx entry: $$b"; stale=1; \
		fi; \
	done; \
	for f in $(SERVER_DIR)/.sqlx/query-*.json; do \
		b=$$(basename $$f); \
		if [ ! -f "/tmp/sqlx-check/$$b" ]; then \
			echo "::error::stale committed .sqlx entry (no longer used): $$b"; stale=1; \
		fi; \
	done; \
	rm -rf /tmp/sqlx-check; \
	if [ "$$stale" = "1" ]; then \
		echo "server/.sqlx/ is out of date — run 'make sqlx-prepare' and commit server/.sqlx/."; \
		exit 1; \
	fi
	@echo "server/.sqlx/ is up to date ($$(ls $(SERVER_DIR)/.sqlx/query-*.json | wc -l) entries)."

skills-manifest-generate: ## Regenerate .djinn/skills.json after editing skills/references
		cd $(SERVER_DIR) && cargo run -p djinn-agent --bin djinn-skills-manifest -- generate --root ..

skills-manifest-check: ## Fail if .djinn/skills.json is stale (CI/local drift guard)
		cd $(SERVER_DIR) && cargo run -p djinn-agent --bin djinn-skills-manifest -- check --root ..

test-db-reset: ## Wipe and restart the test Postgres — cleans out djinn_test_* DBs
	docker compose stop postgres-test
	docker compose rm -sf postgres-test
	docker compose up -d postgres-test
	@$(MAKE) --no-print-directory test-db-migrate
	@$(MAKE) --no-print-directory test-db-postgres-template

test: ## Run djinn-db tests (env routes to :5433 via .cargo/config.toml)
	@$(MAKE) --no-print-directory test-db-migrate test-db-postgres-template test-vault
	cd $(SERVER_DIR) && cargo test -p djinn-db

# Postgres + tmpfs handles the full workspace concurrently without the
# OOM cascade that Dolt's caching exhibited, so we can run the workspace
# in one shot via nextest. Keep this aligned with the merge-queue server-test
# job; it is the local entrypoint for vjs6 lifecycle/concurrency regressions.
test-all: ## Run the merge-queue/full-suite nextest command with template Postgres
	@$(MAKE) --no-print-directory test-vault
	@$(MAKE) --no-print-directory test-db-reset
	cd $(SERVER_DIR) && cargo nextest run --workspace --all-targets --all-features --profile ci

validate-taskrun-backstop: ## Run epic 8451 full Postgres-backed validation
	./scripts/validate-taskrun-backstop.sh

check-boundaries: ## Run architectural boundary checks against the server workspace (no DB / no graph warm)
	python3 scripts/check_boundaries.py
