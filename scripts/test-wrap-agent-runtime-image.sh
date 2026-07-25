#!/usr/bin/env bash

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/bin" "$TMP/artifacts"
printf 'worker fixture\n' > "$TMP/artifacts/djinn-agent-worker"
chmod +x "$TMP/artifacts/djinn-agent-worker"
printf 'launcher fixture\n' > "$TMP/artifacts/djinn-cgroup-launcher"
chmod +x "$TMP/artifacts/djinn-cgroup-launcher"

cat > "$TMP/bin/docker" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$1 $2" == "image inspect" ]]; then
  [[ "${BASE_PRESENT:-0}" == "1" ]]
  exit
fi
printf 'docker %s\n' "$*" >> "$CALL_LOG"
EOF
chmod +x "$TMP/bin/docker"

cat > "$TMP/base-builder" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'base-builder BASE_TAG=%s\n' "$BASE_TAG" >> "$CALL_LOG"
[[ "${BASE_BUILD_FAIL:-0}" != "1" ]]
EOF
chmod +x "$TMP/base-builder"

run_wrapper() {
  PATH="$TMP/bin:$PATH" \
  CALL_LOG="$TMP/calls" \
  ARTIFACTS_DIR="$TMP/artifacts" \
  IMAGE_TAG="localhost:5001/djinn-agent-runtime:test" \
  BASE_BUILD_SCRIPT="$TMP/base-builder" \
  BASE_PRESENT="$1" \
  BASE_BUILD_FAIL="${2:-0}" \
    bash "$REPO_ROOT/scripts/tilt/wrap-agent-runtime-image.sh"
}

assert_log_contains() {
  if ! grep -Fq "$1" "$TMP/calls"; then
    echo "not ok: expected log entry: $1" >&2
    exit 1
  fi
}

assert_log_omits() {
  if grep -Fq "$1" "$TMP/calls"; then
    echo "not ok: unexpected log entry: $1" >&2
    exit 1
  fi
}

: > "$TMP/calls"
run_wrapper 1
assert_log_omits "base-builder"
assert_log_contains "docker build"
assert_log_contains "docker push localhost:5001/djinn-agent-runtime:test"
echo "ok: present base skips rebuild and wraps/pushes the worker"

: > "$TMP/calls"
run_wrapper 0
assert_log_contains "base-builder BASE_TAG=djinn-agent-runtime-base:dev"
assert_log_contains "docker build"
assert_log_contains "docker push localhost:5001/djinn-agent-runtime:test"
echo "ok: missing base rebuilds before wrapping/pushing the worker"

: > "$TMP/calls"
if run_wrapper 0 1; then
  echo "not ok: base rebuild failure should fail the wrapper" >&2
  exit 1
fi
assert_log_contains "base-builder BASE_TAG=djinn-agent-runtime-base:dev"
assert_log_omits "docker build"
assert_log_omits "docker push"
echo "ok: base rebuild failure propagates before wrap/push"

echo "all agent runtime wrapper assertions passed"
