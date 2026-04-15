DESKTOP_DIR := $(CURDIR)/desktop
SERVER_DIR := $(CURDIR)/server

.PHONY: dev help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*##"}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

dev: ## Start the Vite web client (assumes the Dockerized server is already running)
	cd $(DESKTOP_DIR) && pnpm dev
