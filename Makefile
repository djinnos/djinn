BINARY := djinn-server
TARGET_DEBUG := $(CURDIR)/target/debug/$(BINARY)
TARGET_RELEASE := $(CURDIR)/target/release/$(BINARY)
INSTALL_DIR := $(HOME)/.local/bin
INSTALL_PATH := $(INSTALL_DIR)/$(BINARY)

.PHONY: build release run dev test check clippy fmt clean install restart

build:
	cargo build

release:
	cargo build --release

run:
	cargo run

dev:
	cargo watch -x run

test:
	cargo test

check:
	cargo check

clippy:
	cargo clippy -- -D warnings

fmt:
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

clean:
	cargo clean
