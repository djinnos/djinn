#!/bin/sh
# Hermetic end-to-end fixture for scripts/djinn-observability-logs.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
HELPER=$SCRIPT_DIR/djinn-observability-logs
BASE=$(mktemp -d /var/tmp/djinn-observability-logs-test.XXXXXX)
ROOT=$BASE/store
UID=123e4567-e89b-12d3-a456-426614174000
NS=incident
cleanup() { rm -rf -- "$BASE"; }
trap cleanup EXIT HUP INT TERM
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
assert_eq() { [ "$1" = "$2" ] || fail "$3 (expected [$1], got [$2])"; }
run() { DJINN_OBSERVABILITY_ROOT=$ROOT "$HELPER" "$@"; }

[ -x "$HELPER" ] || fail "helper is not executable"
mkdir -p "$ROOT/$NS/$UID/api"
ACTIVE="$ROOT/$NS/$UID/api/20260724T020000Z-000000.jsonl.active"
printf '%s\n' '{"message":"active-before-rotation"}' > "$ACTIVE"
# A deleted pod has no Kubernetes object: only its retained stream tree exists.
assert_eq '{"message":"active-before-rotation"}' "$(run --namespace "$NS" --pod-uid "$UID")" \
    'low-volume active segment is retrievable after deletion'

# Put a fake head before PATH. It appends only after the helper has statted the
# file, proving the helper's bounded read excludes the concurrent suffix.
mkdir "$BASE/bin"
cat > "$BASE/bin/head" <<EOF
#!/bin/sh
printf '%s\\n' '{"message":"concurrent-suffix"}' >> '$ACTIVE'
exec /usr/bin/head "\$@"
EOF
chmod +x "$BASE/bin/head"
PATH="$BASE/bin:$PATH" run --namespace "$NS" --pod-uid "$UID" > "$BASE/prefix.out"
grep -q 'active-before-rotation' "$BASE/prefix.out" || fail 'active prefix was not returned'
if grep -q 'concurrent-suffix' "$BASE/prefix.out"; then
    fail 'concurrent suffix was included despite active length snapshot'
fi

# A prior compressed segment must precede the current active segment by its
# hour/sequence name. The earlier suffix remains in the active store, and is
# intentionally visible to a later retrieval.
printf '%s\n' '{"message":"rotated-gzip"}' > "$BASE/closed.jsonl"
gzip -c "$BASE/closed.jsonl" > "$ROOT/$NS/$UID/api/20260724T010000Z-000000.jsonl.gz"
run --namespace "$NS" --pod-uid "$UID" > "$BASE/ordered.out"
expected=$(cat <<'EOF'
{"message":"rotated-gzip"}
{"message":"active-before-rotation"}
{"message":"concurrent-suffix"}
EOF
)
assert_eq "$expected" "$(cat "$BASE/ordered.out")" 'gzip and active segments are ordered and streamed'

# Explicit fallback accepts a name directory only when it identifies exactly
# one retained pod tree; the UID selector remains the normal mode.
mkdir -p "$ROOT/$NS/deleted-worker/api"
printf '%s\n' '{"message":"pod-name-fallback"}' > "$ROOT/$NS/deleted-worker/api/20260724T030000Z-000000.jsonl.active"
assert_eq '{"message":"pod-name-fallback"}' "$(run --namespace "$NS" --pod-name deleted-worker)" \
    'explicit pod-name fallback retrieves one retained tree'
mkdir -p "$ROOT/$NS/deleted-worker-extra/api"
if run --namespace "$NS" --pod-name deleted-worker > /dev/null 2> "$BASE/ambiguous.err"; then
    fail 'ambiguous pod-name fallback succeeded'
fi
grep -q 'ambiguous' "$BASE/ambiguous.err" || fail 'ambiguous fallback error is not actionable'

printf 'not-a-gzip' > "$ROOT/$NS/$UID/api/20260724T000000Z-000000.jsonl.gz"
if run --namespace "$NS" --pod-uid "$UID" > /dev/null 2> "$BASE/corrupt.err"; then
    fail 'corrupt gzip succeeded'
fi
grep -q 'corrupt gzip segment' "$BASE/corrupt.err" || fail 'corrupt gzip error is not actionable'
if run --namespace ../bad --pod-uid "$UID" > /dev/null 2> "$BASE/invalid.err"; then
    fail 'traversal selector succeeded'
fi
grep -q 'not a path' "$BASE/invalid.err" || fail 'traversal error is not actionable'
printf 'ok - djinn observability log retrieval fixtures\n'
