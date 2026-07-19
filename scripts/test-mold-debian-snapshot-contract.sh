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

# A shared declaration is insufficient unless the command configuring apt's
# additional source actually consumes it. The ordinary Debian source remains
# available for the rest of each image's packages; only mold's dated snapshot
# source may be added.
logical_commands() {
    local file="$1"

    # Join shell/Dockerfile continuations so the source command can be checked
    # as a unit, while ignoring comments that merely document source paths.
    awk '
        {
            line = $0
            sub(/[[:space:]]*#.*/, "", line)
            if (line == "") next
            command = command (command == "" ? "" : " ") line
            if (line ~ /\\[[:space:]]*$/) {
                sub(/\\[[:space:]]*$/, "", command)
                next
            }
            gsub(/[[:space:]]+/, " ", command)
            print command
            command = ""
        }
        END {
            if (command != "") {
                gsub(/[[:space:]]+/, " ", command)
                print command
            }
        }
    ' "$file"
}

verify_snapshot_source() {
    local file="$1"
    local expected_snapshot="$2"
    local snapshot_reference="$3"
    local source_line
    local line
    local source_paths
    local found_source_line=0

    source_line="printf 'deb [check-valid-until=no] %s trixie main\\n' ${snapshot_reference} > /etc/apt/sources.list.d/mold-snapshot.list"
    while IFS= read -r line; do
        [[ "$line" == *'/etc/apt/sources.list'* ]] || continue
        # A logical command can contain several `&&`-joined source writes.
        # Require its sole source-list reference to be the mold snapshot list,
        # rather than accepting a valid write alongside an alternate one.
        source_paths="$(grep -Eo '/etc/apt/sources\.list(\.d/[[:alnum:]_.-]+)?' <<<"$line" || true)"
        [[ "$source_paths" == '/etc/apt/sources.list.d/mold-snapshot.list' ]] \
            || fail "alternate apt source configuration in ${file#$REPO_ROOT/}: $line"
        [[ "$line" == *"$source_line"* ]] \
            || fail "alternate apt source configuration in ${file#$REPO_ROOT/}: $line"
        found_source_line=1
    done < <(logical_commands "$file")
    [[ "$found_source_line" == 1 ]] \
        || fail "apt source does not consume its declared Debian snapshot in ${file#$REPO_ROOT/}"

    # A literal alternate snapshot URL can otherwise bypass the declaration
    # while leaving its value unchanged.
    while IFS= read -r source_url; do
        [[ "$source_url" == "$expected_snapshot" ]] \
            || fail "alternate Debian snapshot in ${file#$REPO_ROOT/}: $source_url"
    done < <(grep -Eo 'https?://snapshot\.debian\.org/archive/debian/[0-9]{8}T[0-9]{6}Z' "$file" || true)
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
[[ "$rust_snapshot" =~ ^http://snapshot\.debian\.org/archive/debian/[0-9]{8}T[0-9]{6}Z$ ]] \
    || fail "snapshot URL is not a dated Debian snapshot: $rust_snapshot"
[[ "$rust_version" != *'${'* && "$rust_version" != *'$'* ]] \
    || fail "mold version must be literal: $rust_version"

verify_snapshot_source "$RUST_INSTALLER" "$rust_snapshot" '"${DEBIAN_SNAPSHOT_URL}"'
verify_snapshot_source "$RUNTIME_DOCKERFILE" "$runtime_snapshot" '"$DEBIAN_SNAPSHOT_URL"'

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
