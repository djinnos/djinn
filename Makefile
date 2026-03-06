BINARY := djinn-server
TARGET_DEBUG := $(CURDIR)/target/debug/$(BINARY)
TARGET_RELEASE := $(CURDIR)/target/release/$(BINARY)
INSTALL_DIR := $(HOME)/.local/bin
INSTALL_PATH := $(INSTALL_DIR)/$(BINARY)
DESKTOP_DIR := $(CURDIR)/../desktop
DAEMON_FILE := $(HOME)/.djinn/daemon.json

.PHONY: build release run dev test check clippy fmt clean install restart run-all reset stop halt help

build: ## Build debug binary
	cargo build

release: ## Build release binary
	cargo build --release

run: ## Run server (foreground)
	cargo run

dev: ## Run server with cargo watch
	cargo watch -x run

test: ## Run tests
	cargo test

check: ## Check compilation
	cargo check

clippy: ## Run clippy lints
	cargo clippy -- -D warnings

fmt: ## Format code
	cargo fmt

install: build
	@mkdir -p $(INSTALL_DIR)
	@if [ -L "$(INSTALL_PATH)" ]; then \
		current=$$(readlink "$(INSTALL_PATH)"); \
		if [ "$$current" != "$(TARGET_DEBUG)" ]; then \
			ln -sf "$(TARGET_DEBUG)" "$(INSTALL_PATH)"; \
			echo "Updated symlink: $(INSTALL_PATH) -> $(TARGET_DEBUG)"; \
		else \
			echo "Symlink already up to date"; \
		fi \
	else \
		if [ -e "$(INSTALL_PATH)" ]; then \
			echo "Error: $(INSTALL_PATH) exists and is not a symlink"; \
			exit 1; \
		fi; \
		ln -s "$(TARGET_DEBUG)" "$(INSTALL_PATH)"; \
		echo "Created symlink: $(INSTALL_PATH) -> $(TARGET_DEBUG)"; \
	fi

restart: build
	@DAEMON_FILE="$$HOME/.djinn/daemon.json"; \
	if [ -f "$$DAEMON_FILE" ]; then \
		PID=$$(jq -r '.pid' "$$DAEMON_FILE" 2>/dev/null); \
		if [ -n "$$PID" ] && [ "$$PID" != "null" ] && kill -0 "$$PID" 2>/dev/null; then \
			echo "Killing djinn-server (pid=$$PID)"; \
			kill "$$PID"; \
			while kill -0 "$$PID" 2>/dev/null; do sleep 0.1; done; \
		fi; \
	fi
	$(TARGET_DEBUG) --ensure-daemon

run-all: ## Start desktop app (server runs as sidecar)
	-@pkill -f "djinn-server" 2>/dev/null || true
	@sleep 0.5
	cd $(DESKTOP_DIR) && $(MAKE) run-all

stop: ## Stop the server daemon
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

halt: stop ## Stop everything (server + desktop)
	-@pkill -f "djinn-server" 2>/dev/null || true
	-@pkill -f "djinnos-desktop" 2>/dev/null || true
	-@pkill -f "tauri dev" 2>/dev/null || true
	-@pkill -f "vite.*desktop" 2>/dev/null || true
	@echo "All processes halted."

reset: ## Kill everything and restart fresh
	@echo "Killing all djinn processes..."
	-@pkill -f "djinn-server" 2>/dev/null || true
	-@pkill -f "tauri dev" 2>/dev/null || true
	-@pkill -f "djinnos-desktop" 2>/dev/null || true
	-@pkill -f "vite.*desktop" 2>/dev/null || true
	@sleep 1
	@echo "Restarting..."
	$(MAKE) run-all

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*##"}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

clean:
	cargo clean
