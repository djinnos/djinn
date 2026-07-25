#!/bin/sh
# Hermetic real-process recovery fixture for djinn-log-rotator.
#
# This deliberately uses the fixed localhost listeners owned by the binary, so
# it must run serially with other rotator runtime fixtures.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
SERVER_ROOT=$REPO_ROOT/server
HELPER=$SCRIPT_DIR/djinn-observability-logs
BASE=$(mktemp -d /var/tmp/djinn-log-rotator-runtime.XXXXXX)
ROOT=$BASE/store
LOG=$BASE/rotator.log
PID=
UID=550e8400-e29b-41d4-a716-446655440000
NAMESPACE=runtime
CONTAINER=api

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    [ ! -f "$LOG" ] || { printf '%s\n' '--- rotator log ---' >&2; cat "$LOG" >&2; }
    exit 1
}

stop_rotator() {
    [ -n "${PID:-}" ] || return 0
    if kill -0 "$PID" 2>/dev/null; then
        kill -INT "$PID" 2>/dev/null || true
        attempts=0
        while kill -0 "$PID" 2>/dev/null && [ "$attempts" -lt 50 ]; do
            sleep 0.1
            attempts=$((attempts + 1))
        done
        if kill -0 "$PID" 2>/dev/null; then
            kill -KILL "$PID" 2>/dev/null || true
        fi
    fi
    wait "$PID" 2>/dev/null || true
    PID=
}

cleanup() {
    stop_rotator
    rm -rf -- "$BASE"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

[ -x "$HELPER" ] || fail "retrieval helper is not executable: $HELPER"

# A bind probe produces a useful error before the child is started; curl alone
# would otherwise report an unrelated health failure when another fixture owns
# either of the binary's fixed listeners.
assert_ports_available() {
    python3 - <<'PY' || exit 1
import socket
import sys
for port in (8687, 9091):
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        listener.bind(("127.0.0.1", port))
    except OSError as error:
        print(
            "djinn-log-rotator runtime fixture requires serial execution: "
            f"127.0.0.1:{port} is unavailable ({error})",
            file=sys.stderr,
        )
        sys.exit(1)
    finally:
        listener.close()
PY
}

if [ -n "${DJINN_LOG_ROTATOR_BIN:-}" ]; then
    BIN=$DJINN_LOG_ROTATOR_BIN
else
    BIN=${CARGO_TARGET_DIR:-$SERVER_ROOT/target}/debug/djinn-log-rotator
    if [ ! -x "$BIN" ]; then
        (cd "$SERVER_ROOT" && cargo build -p djinn-log-rotator) || fail 'could not build djinn-log-rotator'
    fi
fi
[ -x "$BIN" ] || fail "rotator binary is not executable: $BIN (set DJINN_LOG_ROTATOR_BIN to override)"

start_rotator() {
    assert_ports_available || fail 'fixed rotator port is already in use; run this fixture serially'
    DJINN_LOG_STORE_DIR=$ROOT "$BIN" >"$LOG" 2>&1 &
    PID=$!
    attempts=0
    while [ "$attempts" -lt 100 ]; do
        if curl --silent --show-error --fail --max-time 1 http://127.0.0.1:8687/healthz >/dev/null 2>&1 && \
            curl --silent --show-error --fail --max-time 1 http://127.0.0.1:9091/healthz >/dev/null 2>&1; then
            return 0
        fi
        if ! kill -0 "$PID" 2>/dev/null; then
            wait "$PID" 2>/dev/null || true
            PID=
            fail 'rotator exited before localhost health became ready'
        fi
        sleep 0.1
        attempts=$((attempts + 1))
    done
    fail 'rotator did not become healthy on 127.0.0.1:8687 and :9091'
}

post_record() {
    status=$(curl --silent --show-error --max-time 3 -o /dev/null -w '%{http_code}' \
        -H 'content-type: application/json' --data "$1" http://127.0.0.1:8687/ingest) || fail 'POST /ingest failed'
    [ "$status" = 204 ] || fail "POST /ingest returned HTTP $status, expected 204"
}

assert_messages() {
    output=$1
    expected=$2
    python3 - "$output" "$expected" <<'PY'
import json
import sys
path, expected = sys.argv[1:]
raw = open(path, "rb").read()
if not raw.endswith(b"\n"):
    raise SystemExit("retrieval did not end on a complete JSON-line boundary")
lines = raw.splitlines()
if not lines:
    raise SystemExit("retrieval unexpectedly returned no records")
try:
    messages = [json.loads(line)["message"] for line in lines]
except (json.JSONDecodeError, KeyError) as error:
    raise SystemExit(f"retrieval contained a non-JSON complete line: {error}")
if messages != expected.split(","):
    raise SystemExit(f"unexpected retrieval messages: {messages!r}")
PY
}

assert_store_contract() {
    active=$(find "$ROOT/$NAMESPACE/$UID/$CONTAINER" -maxdepth 1 -type f -name '*.jsonl.active' -print)
    [ -n "$active" ] || fail 'active segment was not retained'
    [ "$(printf '%s\n' "$active" | wc -l)" -eq 1 ] || fail 'expected exactly one active segment'
    sidecar=$active.bytes
    [ -f "$sidecar" ] || fail 'active segment logical-byte sidecar is missing'
    for directory in "$ROOT" "$ROOT/$NAMESPACE" "$ROOT/$NAMESPACE/$UID" "$ROOT/$NAMESPACE/$UID/$CONTAINER"; do
        [ "$(stat -c '%a' "$directory")" = 750 ] || fail "directory mode is not 0750: $directory"
    done
    for file in "$active" "$sidecar"; do
        [ "$(stat -c '%a' "$file")" = 640 ] || fail "file mode is not 0640: $file"
    done
    physical=$(wc -c < "$active" | tr -d '[:space:]')
    logical=$(tr -d '[:space:]' < "$sidecar")
    [ "$physical" = "$logical" ] || fail "logical sidecar ($logical) does not match complete active bytes ($physical)"
}

start_rotator
curl --silent --show-error --fail http://127.0.0.1:8687/healthz >/dev/null || fail 'ingest health endpoint failed'
curl --silent --show-error --fail http://127.0.0.1:9091/healthz >/dev/null || fail 'metrics health endpoint failed'
curl --silent --show-error --fail http://127.0.0.1:9091/metrics > "$BASE/metrics.before" || fail 'metrics endpoint failed'
grep -q '^djinn_log_store_writable 1$' "$BASE/metrics.before" || fail 'metrics did not report writable store'
grep -q '^djinn_log_rotator_build_info{' "$BASE/metrics.before" || fail 'metrics did not report build info'

FIRST='{"timestamp":"2026-07-25T12:00:00Z","namespace":"runtime","pod_name":"api-0","pod_uid":"550e8400-e29b-41d4-a716-446655440000","container":"api","stream":"stdout","message":"before-restart-one"}'
SECOND='{"timestamp":"2026-07-25T12:00:01Z","namespace":"runtime","pod_name":"api-0","pod_uid":"550e8400-e29b-41d4-a716-446655440000","container":"api","stream":"stdout","message":"before-restart-two"}'
THIRD='{"timestamp":"2026-07-25T12:00:02Z","namespace":"runtime","pod_name":"api-0","pod_uid":"550e8400-e29b-41d4-a716-446655440000","container":"api","stream":"stdout","message":"after-restart"}'
post_record "$FIRST"
post_record "$SECOND"
DJINN_OBSERVABILITY_ROOT=$ROOT "$HELPER" --namespace "$NAMESPACE" --pod-uid "$UID" > "$BASE/before-restart.out"
assert_messages "$BASE/before-restart.out" 'before-restart-one,before-restart-two'
assert_store_contract

stop_rotator
start_rotator
curl --silent --show-error --fail http://127.0.0.1:9091/metrics > "$BASE/metrics.after" || fail 'metrics endpoint failed after restart'
grep -q '^djinn_log_store_writable 1$' "$BASE/metrics.after" || fail 'restarted metrics did not report writable store'
DJINN_OBSERVABILITY_ROOT=$ROOT "$HELPER" --namespace "$NAMESPACE" --pod-uid "$UID" > "$BASE/recovered.out"
assert_messages "$BASE/recovered.out" 'before-restart-one,before-restart-two'
post_record "$THIRD"
DJINN_OBSERVABILITY_ROOT=$ROOT "$HELPER" --namespace "$NAMESPACE" --pod-uid "$UID" > "$BASE/after-restart.out"
assert_messages "$BASE/after-restart.out" 'before-restart-one,before-restart-two,after-restart'
assert_store_contract

printf 'ok - djinn log rotator real-process restart and retrieval fixture\n'
