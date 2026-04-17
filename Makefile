DESKTOP_DIR := $(CURDIR)/desktop
SERVER_DIR := $(CURDIR)/server

.PHONY: help up up-no-build down logs dev watch test test-all test-vault \
	kind-up kind-down image image-push-local helm-install-local helm-uninstall

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
# Kubernetes / Helm local-dev inner loop (Phase 2 PR 4).
# ----------------------------------------------------------------------------

KIND_CLUSTER_NAME ?= djinn
LOCAL_REGISTRY    ?= localhost:5001
DJINN_IMAGE_TAG   ?= dev

kind-up: ## Create the local kind cluster + registry (idempotent)
	CLUSTER_NAME=$(KIND_CLUSTER_NAME) bash scripts/kind/setup-kind.sh

kind-down: ## Delete the local kind cluster (registry container survives)
	kind delete cluster --name $(KIND_CLUSTER_NAME)

image: ## Build djinn-server + djinn-agent-runtime container images
	docker build -f Dockerfile -t djinn-server:$(DJINN_IMAGE_TAG) .
	docker build -f server/docker/djinn-agent-runtime.Dockerfile -t djinn-agent-runtime:$(DJINN_IMAGE_TAG) .

image-push-local: image ## Retag images for the local kind registry and push
	docker tag djinn-server:$(DJINN_IMAGE_TAG)         $(LOCAL_REGISTRY)/djinn-server:$(DJINN_IMAGE_TAG)
	docker tag djinn-agent-runtime:$(DJINN_IMAGE_TAG)  $(LOCAL_REGISTRY)/djinn-agent-runtime:$(DJINN_IMAGE_TAG)
	docker push $(LOCAL_REGISTRY)/djinn-server:$(DJINN_IMAGE_TAG)
	docker push $(LOCAL_REGISTRY)/djinn-agent-runtime:$(DJINN_IMAGE_TAG)

helm-install-local: ## Install djinn-crds + djinn into the local kind cluster
	helm upgrade --install djinn-crds deploy/helm/djinn-crds
	helm upgrade --install djinn deploy/helm/djinn \
		--values deploy/helm/djinn/values.local.yaml \
		--namespace djinn --create-namespace

helm-uninstall: ## Remove djinn + djinn-crds releases
	-helm uninstall djinn --namespace djinn
	-helm uninstall djinn-crds
