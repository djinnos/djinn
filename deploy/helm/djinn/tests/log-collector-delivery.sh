#!/bin/sh
# Hermetic rendered-Vector delivery fixture using the real rotator process.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
CHART_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../../../.." && pwd)
RUNTIME_DIR=$REPO_ROOT/scripts
RUNTIME_HARNESS=$RUNTIME_DIR/test-djinn-log-rotator-runtime.sh
for tool in helm python3 vector curl gzip; do
    command -v "$tool" >/dev/null 2>&1 || { printf 'FAIL: %s is required for the rendered Vector delivery fixture\n' "$tool" >&2; exit 1; }
done
[ -r "$RUNTIME_HARNESS" ] || { printf 'FAIL: rotator runtime harness is missing: %s\n' "$RUNTIME_HARNESS" >&2; exit 1; }

# Source wm89's lifecycle and fixed-port diagnostics, without its standalone test.
DJINN_LOG_ROTATOR_RUNTIME_LIBRARY=1 DJINN_LOG_ROTATOR_RUNTIME_SCRIPT_DIR=$RUNTIME_DIR . "$RUNTIME_HARNESS"
SOURCE=$BASE/source-pods
ROOT=$BASE/retained-store
VECTOR_DATA=$BASE/vector-data
VECTOR_LOG=$BASE/vector.log
VECTOR_PID=
NAMESPACE=delivery
POD_NAME=api-0
UID=550e8400-e29b-41d4-a716-446655440000
CONTAINER=api
CRI_DIR=$SOURCE/${NAMESPACE}_${POD_NAME}_${UID}/$CONTAINER

stop_vector() {
    [ -n "${VECTOR_PID:-}" ] || return 0
    if kill -0 "$VECTOR_PID" 2>/dev/null; then
        kill -TERM "$VECTOR_PID" 2>/dev/null || true
        attempts=0
        while kill -0 "$VECTOR_PID" 2>/dev/null && [ "$attempts" -lt 50 ]; do sleep 0.1; attempts=$((attempts + 1)); done
        kill -0 "$VECTOR_PID" 2>/dev/null && kill -KILL "$VECTOR_PID" 2>/dev/null || true
    fi
    wait "$VECTOR_PID" 2>/dev/null || true
    VECTOR_PID=
}
cleanup() { stop_vector; stop_rotator; rm -rf -- "$BASE"; }
trap cleanup EXIT
fail() {
    printf 'FAIL: %s\n' "$*" >&2
    [ ! -f "$VECTOR_LOG" ] || { printf '%s\n' '--- vector log ---' >&2; cat "$VECTOR_LOG" >&2; }
    [ ! -f "$LOG" ] || { printf '%s\n' '--- rotator log ---' >&2; cat "$LOG" >&2; }
    exit 1
}

mkdir -p "$CRI_DIR" "$VECTOR_DATA"
python3 - "$CRI_DIR/0.log" <<'PY'
import json, sys
records = [
    ("stdout", json.dumps({"request": {"Authorization": "top-secret", "body": "keep"}, "items": [{"Api-Key": "nested-secret"}]}, separators=(",", ":"))),
    ("stderr", json.dumps({"query": "€" * 683, "detail": "uncapped"}, ensure_ascii=False, separators=(",", ":"))),
    ("stdout", "plain token=not-an-assignment"),
    ("stderr", 'djinn.panic_summary.v1 {"token":"panic-secret"}'),
]
with open(sys.argv[1], "w", encoding="utf-8") as out:
    for n, (stream, message) in enumerate(records):
        out.write(f"2026-07-25T12:00:0{n}Z {stream} F {message}\n")
PY

helm template fixture "$CHART_DIR" --set logCollector.enabled=true \
    --set logCollector.rotatorImage=example/rotator:1.2.3 \
    --set logCollector.vectorImage=example/vector:0.43.1 \
    --show-only templates/configmap-log-collector.yaml > "$BASE/rendered.yaml" || fail 'Helm failed to render collector ConfigMap'
python3 - "$BASE/rendered.yaml" "$BASE/vector.yaml" "$SOURCE" "$VECTOR_DATA" <<'PY'
import sys
from pathlib import Path
rendered, output, source, data = map(Path, sys.argv[1:])
lines = rendered.read_text().splitlines()
try: start = lines.index("  vector.yaml: |") + 1
except ValueError as error: raise SystemExit("rendered ConfigMap has no vector.yaml") from error
content = []
for line in lines[start:]:
    if line.startswith("    "): content.append(line[4:])
    elif line: break
config = "\n".join(content) + "\n"
if "uri: http://127.0.0.1:8687/ingest" not in config: raise SystemExit("rendered Vector HTTP sink was not retained")
config = config.replace("data_dir: /var/lib/vector", f"data_dir: {data}", 1)
config = config.replace("- /source/pods/*/*/*.log", f"- {source}/*/*/*.log", 1)
if "/source/pods" in config or "/store" in config: raise SystemExit("fixture redirection changed an unexpected mount boundary")
output.write_text(config)
PY
grep -Fq "$ROOT" "$BASE/vector.yaml" && fail 'Vector configuration exposes the retained store'

start_rotator
vector --config "$BASE/vector.yaml" >"$VECTOR_LOG" 2>&1 &
VECTOR_PID=$!
for attempt in $(seq 1 100); do
    count=$(find "$ROOT/$NAMESPACE/$UID/$CONTAINER" -name '*.jsonl.active' -exec cat {} + 2>/dev/null | wc -l | tr -d ' ' || true)
    [ "$count" = 4 ] && break
    kill -0 "$VECTOR_PID" 2>/dev/null || fail 'Vector exited before delivering CRI records'
    sleep 0.1
done
[ "${count:-0}" = 4 ] || fail "Vector delivered $count records, expected 4"

DJINN_OBSERVABILITY_ROOT=$ROOT "$HELPER" --namespace "$NAMESPACE" --pod-uid "$UID" > "$BASE/delivered.jsonl"
python3 - "$BASE/delivered.jsonl" <<'PY'
import json, sys
records = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8")]
messages = [record["message"] for record in records]
assert len(records) == 4
assert json.loads(messages[0])["request"]["Authorization"] == "***REDACTED***"
assert json.loads(messages[0])["items"][0]["Api-Key"] == "***REDACTED***"
assert json.loads(messages[1])["query"] == "€" * 682 + "…[FIELD_TRUNCATED original_bytes=2049]"
assert json.loads(messages[1])["detail"] == "uncapped"
assert messages[2] == "plain token=not-an-assignment"
assert messages[3] == 'djinn.panic_summary.v1 {"token":"panic-secret"}'
for record in records:
    assert (record["namespace"], record["pod_name"], record["pod_uid"], record["container"]) == ("delivery", "api-0", "550e8400-e29b-41d4-a716-446655440000", "api")
PY

# Deleting the only source demonstrates retained active data is independent of pods.
rm -rf -- "$SOURCE"
DJINN_OBSERVABILITY_ROOT=$ROOT "$HELPER" --namespace "$NAMESPACE" --pod-uid "$UID" > "$BASE/after-delete.jsonl"
cmp -s "$BASE/delivered.jsonl" "$BASE/after-delete.jsonl" || fail 'active records changed after source deletion'

# An earlier compressed segment in the exact retained stream must precede active data.
STREAM_DIR=$ROOT/$NAMESPACE/$UID/$CONTAINER
printf '%s\n' '{"message":"earlier-gzip"}' > "$BASE/earlier.jsonl"
gzip -c "$BASE/earlier.jsonl" > "$STREAM_DIR/00000101T000000Z-000000.jsonl.gz"
DJINN_OBSERVABILITY_ROOT=$ROOT "$HELPER" --namespace "$NAMESPACE" --pod-uid "$UID" > "$BASE/ordered.jsonl"
python3 - "$BASE/ordered.jsonl" "$BASE/delivered.jsonl" <<'PY'
import json, sys
ordered = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8")]
delivered = [json.loads(line) for line in open(sys.argv[2], encoding="utf-8")]
assert ordered[0]["message"] == "earlier-gzip"
assert ordered[1:] == delivered
PY

# Both processes restart while the source is absent: acknowledged complete lines stay once.
stop_vector
stop_rotator
start_rotator
vector --config "$BASE/vector.yaml" >"$VECTOR_LOG" 2>&1 &
VECTOR_PID=$!
sleep 1
kill -0 "$VECTOR_PID" 2>/dev/null || fail 'Vector exited during collector restart'
DJINN_OBSERVABILITY_ROOT=$ROOT "$HELPER" --namespace "$NAMESPACE" --pod-uid "$UID" > "$BASE/restarted.jsonl"
cmp -s "$BASE/ordered.jsonl" "$BASE/restarted.jsonl" || fail 'restart lost or duplicated an accepted complete record'
printf 'ok - rendered Vector to rotator delivery, retrieval, and restart fixture\n'
