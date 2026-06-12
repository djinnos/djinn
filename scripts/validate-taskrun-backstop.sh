#!/bin/sh
# Full repository validation path for epic 8451 task-run teardown proof.
#
# This is intentionally a repository-level manual/CI entrypoint for hosts that
# have Docker/Postgres available. It provisions docker-compose.yml's
# postgres-test service on 127.0.0.1:5433, builds the djinn_test_template clone
# database, creates the test vault key, then runs the proposal validation set:
# cargo build, strict clippy, and full workspace nextest.

set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SERVER_DIR=$ROOT_DIR/server
LOG_DIR=${LOG_DIR:-$ROOT_DIR/.taskrun-backstop-validation}
TIMESTAMP=$(date -u +%Y%m%dT%H%M%SZ)
LOG_FILE=${LOG_FILE:-$LOG_DIR/validation-$TIMESTAMP.log}

usage() {
    cat <<EOF
Usage: [LOG_DIR=path] $0

Runs the full Postgres-backed validation workflow required by epic 8451:
  1. docker compose up -d postgres-test
  2. apply djinn-db migrations and rebuild djinn_test_template
  3. create /var/tmp/djinn-test-vault/vault.key
  4. cd server && cargo build
  5. cd server && cargo clippy --workspace --all-targets --all-features -- -D warnings
  6. cd server && cargo nextest run --workspace --all-targets --all-features

Prerequisites:
  docker with compose support, cargo, cargo-nextest, sqlx-cli, openssl.

The command prints output to stdout and also writes a timestamped log under
  $LOG_DIR
EOF
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    usage
    exit 0
fi

mkdir -p "$LOG_DIR"

log() {
    printf '\n[%s] %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"
}

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        printf 'ERROR: required command not found: %s\n' "$1" >&2
        return 127
    fi
}

run() {
    log "RUN: $*"
    "$@"
}

main() {
    log "Task-run backstop validation started"
    log "Repository: $ROOT_DIR"
    log "Log file: $LOG_FILE"

    require_cmd docker || return $?
    require_cmd cargo || return $?
    require_cmd cargo-nextest || return $?
    require_cmd sqlx || return $?
    require_cmd openssl || return $?

    # Fail early with a clear environmental blocker if Docker is installed but
    # the daemon/socket is unavailable in the current worker or CI host.
    run docker version || return $?
    run docker compose version || return $?

    # Start the repo-defined Postgres service before any sqlx macro validation
    # or test execution, then wipe leftovers, apply migrations, and create
    # djinn_test_template for fast per-test database clones.
    run docker compose -f "$ROOT_DIR/docker-compose.yml" up -d postgres-test || return $?
    run docker compose -f "$ROOT_DIR/docker-compose.yml" stop postgres-test || return $?
    run docker compose -f "$ROOT_DIR/docker-compose.yml" rm -sf postgres-test || return $?
    run docker compose -f "$ROOT_DIR/docker-compose.yml" up -d postgres-test || return $?

    until docker exec djinn-postgres-test pg_isready -U postgres >/dev/null 2>&1; do
        log "waiting for postgres-test..."
        sleep 1
    done

    run sh -c "cd '$SERVER_DIR/crates/djinn-db' && DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5433/djinn sqlx migrate run --source migrations_postgres" || return $?
    run docker exec djinn-postgres-test psql -U postgres -d postgres -v ON_ERROR_STOP=1 -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='djinn_test_template' AND pid <> pg_backend_pid()" || return $?
    run docker exec djinn-postgres-test psql -U postgres -d postgres -v ON_ERROR_STOP=1 -c "DROP DATABASE IF EXISTS djinn_test_template" || return $?
    run docker exec djinn-postgres-test psql -U postgres -d postgres -v ON_ERROR_STOP=1 -c "CREATE DATABASE djinn_test_template" || return $?
    run sh -c "cd '$SERVER_DIR/crates/djinn-db' && DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5433/djinn_test_template sqlx migrate run --source migrations_postgres" || return $?
    run docker exec djinn-postgres-test psql -U postgres -d postgres -v ON_ERROR_STOP=1 -c "UPDATE pg_database SET datistemplate = TRUE WHERE datname = 'djinn_test_template'" || return $?

    run mkdir -p /var/tmp/djinn-test-vault || return $?
    if [ ! -f /var/tmp/djinn-test-vault/vault.key ]; then
        run openssl rand -out /var/tmp/djinn-test-vault/vault.key 32 || return $?
        run chmod 600 /var/tmp/djinn-test-vault/vault.key || return $?
    fi

    # Some git-facing tests require an identity even when they do not push.
    run git config --global user.email ci@test.local || return $?
    run git config --global user.name "CI Test" || return $?

    run sh -c "cd '$SERVER_DIR' && cargo build" || return $?
    run sh -c "cd '$SERVER_DIR' && cargo clippy --workspace --all-targets --all-features -- -D warnings" || return $?
    run sh -c "cd '$SERVER_DIR' && cargo nextest run --workspace --all-targets --all-features" || return $?

    log "Task-run backstop validation passed"
}

set +e
main >"$LOG_FILE" 2>&1
status=$?
cat "$LOG_FILE"
set -e
if [ "$status" -ne 0 ]; then
    log "Task-run backstop validation failed with exit=$status; see $LOG_FILE" | tee -a "$LOG_FILE"
fi
exit "$status"
