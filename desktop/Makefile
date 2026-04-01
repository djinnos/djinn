SERVER_DIR := $(CURDIR)/../server
DAEMON_FILE := $(HOME)/.djinn/daemon.json

.PHONY: install dev desktop server server-stop run-all reset stop halt server-link storybook build clean help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*##"}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

install: ## Install all dependencies (JS + server build)
	pnpm install
	cd $(SERVER_DIR) && cargo build

dev: ## Run frontend only (browser, no Tauri)
	pnpm dev

desktop: ## Run full Electron desktop app (dev mode)
	pnpm electron:start

server: ## Start the djinn server daemon
	cd $(SERVER_DIR) && make restart

server-stop: ## Stop the djinn server daemon
	@if [ -f "$(DAEMON_FILE)" ]; then \
		PID=$$(jq -r '.pid' "$(DAEMON_FILE)" 2>/dev/null); \
		if [ -n "$$PID" ] && [ "$$PID" != "null" ] && kill -0 "$$PID" 2>/dev/null; then \
			echo "Stopping djinn-server (pid=$$PID)"; \
			kill "$$PID"; \
			while kill -0 "$$PID" 2>/dev/null; do sleep 0.1; done; \
			echo "Stopped."; \
		else \
			echo "Server not running."; \
		fi \
	else \
		echo "No daemon file found."; \
	fi

stop: ## Stop the desktop app (Electron + Vite)
	@echo "Stopping desktop..."
	-@pkill -f "djinnos-desktop" 2>/dev/null || true
	-@pkill -f "electron.*main" 2>/dev/null || true
	-@pkill -f "vite.*desktop" 2>/dev/null || true
	@echo "Desktop stopped."

halt: stop server-stop ## Stop everything (desktop + server)
	-@pkill -f "djinn-server" 2>/dev/null || true
	@echo "All processes halted."

run-all: ## Start desktop app (server auto-downloads if needed)
	-@pkill -f "djinn-server" 2>/dev/null || true
	@sleep 0.5
	pnpm electron:start

reset: ## Kill everything and restart fresh
	@echo "Killing all djinn processes..."
	-@pkill -f "djinn-server" 2>/dev/null || true
	-@pkill -f "electron.*main" 2>/dev/null || true
	-@pkill -f "djinnos-desktop" 2>/dev/null || true
	-@pkill -f "vite.*desktop" 2>/dev/null || true
	@sleep 1
	@echo "Restarting..."
	$(MAKE) run-all

storybook: ## Run Storybook
	pnpm storybook

build: ## Build production Electron app
	pnpm electron:build

clean: ## Remove build artifacts
	rm -rf dist dist-electron release
