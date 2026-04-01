DESKTOP_DIR := $(CURDIR)/desktop
SERVER_DIR := $(CURDIR)/server
DAEMON_FILE := $(HOME)/.djinn/daemon.json

.PHONY: dev help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*##"}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

SERVER_BIN := $(SERVER_DIR)/target/debug/djinn-server

dev: ## Build server, kill daemon, and start Tauri desktop app
	cd $(SERVER_DIR) && cargo build --bin djinn-server
	@if [ -f "$(DAEMON_FILE)" ]; then \
		PID=$$(jq -r '.pid' "$(DAEMON_FILE)" 2>/dev/null); \
		if [ -n "$$PID" ] && [ "$$PID" != "null" ] && kill -0 "$$PID" 2>/dev/null; then \
			echo "Stopping djinn-server (pid=$$PID)..."; \
			kill "$$PID"; \
			while kill -0 "$$PID" 2>/dev/null; do sleep 0.1; done; \
			echo "Stopped."; \
		else \
			echo "Server not running."; \
		fi \
	else \
		echo "No daemon file found."; \
	fi
	cd $(DESKTOP_DIR) && DJINN_SERVER_BIN=$(SERVER_BIN) pnpm tauri:dev
