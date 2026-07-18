#!/usr/bin/env bash
# Enforce the shared, reproducible mold package contract for both image paths.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUST_INSTALLER="$REPO_ROOT/server/crates/djinn-image-builder/scripts/install-rust.sh"
RUNTIME_DOCKERFILE="$REPO_ROOT/server/docker/djinn-agent-runtime-base.Dockerfile"

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

extract_value() {
    local file="$1"
    local name="$2"
    sed -nE "s/^[[:space:]]*(readonly[[:space:]]+|ARG[[:space:]]+)?${name}=\\\"?([^\\\"[:space:]]+)\\\"?.*/\\2/p" "$file" \
        | head -n1
}

rust_snapshot="$(extract_value "$RUST_INSTALLER" DEBIAN_SNAPSHOT_URL)"
runtime_snapshot="$(extract_value "$RUNTIME_DOCKERFILE" DEBIAN_SNAPSHOT_URL)"
rust_version="$(extract_value "$RUST_INSTALLER" MOLD_VERSION)"
runtime_version="$(extract_value "$RUNTIME_DOCKERFILE" MOLD_VERSION)"

for value_name in rust_snapshot runtime_snapshot rust_version runtime_version; do
    value="${!value_name}"
    [[ -n "$value" ]] || fail "$value_name is missing"
done

[[ "$rust_snapshot" == "$runtime_snapshot" ]] \
    || fail "Debian snapshot drift: $rust_snapshot != $runtime_snapshot"
[[ "$rust_version" == "$runtime_version" ]] \
    || fail "mold version drift: $rust_version != $runtime_version"
[[ "$rust_snapshot" =~ ^https://snapshot\.debian\.org/archive/debian/[0-9]{8}T[0-9]{6}Z$ ]] \
    || fail "snapshot URL is not a dated Debian snapshot: $rust_snapshot"
[[ "$rust_version" != *'${'* && "$rust_version" != *'$'* ]] \
    || fail "mold version must be literal: $rust_version"

grep -Fq "mold=${rust_version}" "$RUST_INSTALLER" \
    || fail "Rust installer does not install mold with its literal exact version"
grep -Fq "mold=${runtime_version}" "$RUNTIME_DOCKERFILE" \
    || fail "runtime Dockerfile does not install mold with its literal exact version"

# A bare `mold` token in an apt install command would provide an unpinned
# fallback. Follow Docker/shell line continuations so multi-line installs are
# checked too.
for file in "$RUST_INSTALLER" "$RUNTIME_DOCKERFILE"; do
    awk -v expected="mold=${rust_version}" '
        /apt-get install/ { installing = 1 }
        installing {
            line = $0
            sub(/#.*/, "", line)
            count = split(line, words, /[[:space:]\\]+/)
            for (i = 1; i <= count; i++) {
                if (words[i] == "mold") exit 1
                if (words[i] ~ /^mold=/ && words[i] != expected) exit 1
            }
            if (line !~ /\\[[:space:]]*$/) installing = 0
        }
        END { if (installing) exit 1 }
    ' "$file" || fail "unpinned or divergent mold install in ${file#$REPO_ROOT/}"
done

for file in "$RUST_INSTALLER" "$RUNTIME_DOCKERFILE"; do
    if grep -Eq '(^|[^[:alnum:]_])(NPROC|MOLD_JOBS)([^[:alnum:]_]|$)' "$file"; then
        fail "forbidden NPROC or MOLD_JOBS hint in ${file#$REPO_ROOT/}"
    fi
done

printf 'ok: mold=%s from %s is pinned in both image paths\n' "$rust_version" "$rust_snapshot"
