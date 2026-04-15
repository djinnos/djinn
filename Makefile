DESKTOP_DIR := $(CURDIR)/desktop

.PHONY: help up down logs dev

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*##"}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

up: ## Start the docker compose stack (djinn-server, dolt, qdrant)
	docker compose up -d

down: ## Stop the docker compose stack (volumes persist)
	docker compose down

logs: ## Tail djinn-server logs
	docker compose logs -f djinn-server

dev: ## Launch Electron UI in dev mode (assumes `make up` was run)
	cd $(DESKTOP_DIR) && pnpm electron:start
