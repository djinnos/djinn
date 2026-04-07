DESKTOP_DIR := $(CURDIR)/desktop
SERVER_DIR := $(CURDIR)/server
DAEMON_FILE := $(HOME)/.djinn/daemon.json

.PHONY: dev help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*##"}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

dev: ## Build server and start Electron desktop app (RESTART_SERVER=1 to kill daemon first)
	cd $(SERVER_DIR) && cargo build --bin djinn-server
	@if [ "$(RESTART_SERVER)" = "1" ]; then \
		if [ -f "$(DAEMON_FILE)" ]; then \
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
		fi \
	fi
	@# Resolve the honest target directory from `cargo metadata` so env
	@# vars like CARGO_TARGET_DIR or a `.cargo/config.toml` target-dir
	@# override don't leave DJINN_SERVER_BIN pointing at a stale binary.
	TARGET_DIR=$$(cd $(SERVER_DIR) && cargo metadata --format-version 1 --no-deps | jq -r .target_directory) && \
		SERVER_BIN="$$TARGET_DIR/debug/djinn-server" && \
		echo "Launching electron with DJINN_SERVER_BIN=$$SERVER_BIN" && \
		cd $(DESKTOP_DIR) && DJINN_SERVER_BIN="$$SERVER_BIN" pnpm electron:start
