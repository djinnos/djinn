#!/usr/bin/env bash
# Canonical content fingerprinting for Tilt build inputs.
#
# Build caches must invalidate on content changes, additions, and deletions.
# mtimes cannot provide that contract: deleted files disappear from a
# `find -newer` query, and clock skew can make changed inputs look older than
# the success stamp. Git's blob hashing is available on every supported dev
# machine and gives us one portable content-based implementation.

set -euo pipefail

tilt_input_fingerprint() (
    set -euo pipefail

    local repo_root="$1"
    shift

    repo_root="$(cd "$repo_root" && pwd)"
    local paths_file hashes_file
    paths_file="$(mktemp "${TMPDIR:-/tmp}/djinn-tilt-inputs.XXXXXX")"
    hashes_file="$(mktemp "${TMPDIR:-/tmp}/djinn-tilt-hashes.XXXXXX")"
    trap 'rm -f "${paths_file:-}" "${hashes_file:-}"' EXIT INT TERM

    {
        local input path
        for input in "$@"; do
            if [[ -d "$input" ]]; then
                input="$(cd "$input" && pwd)"
            elif [[ -f "$input" ]]; then
                input="$(cd "$(dirname "$input")" && pwd)/$(basename "$input")"
            fi
            case "$input" in
                "$repo_root" | "$repo_root"/*) ;;
                *)
                    echo "error: build input is outside repository root: $input" >&2
                    exit 2
                    ;;
            esac

            if [[ -f "$input" ]]; then
                printf '%s\n' "${input#"$repo_root"/}"
            elif [[ -d "$input" ]]; then
                while IFS= read -r path; do
                    printf '%s\n' "${path#"$repo_root"/}"
                done < <(
                    find "$input" \
                        \( -type d \( \
                            -name .git -o \
                            -name node_modules -o \
                            -name target -o \
                            -name test-tmp \
                        \) -prune \) -o \
                        -type f -print
                )
            fi
        done
    } | LC_ALL=C sort -u > "$paths_file"

    cd "$repo_root"
    git hash-object --stdin-paths < "$paths_file" > "$hashes_file"
    paste "$hashes_file" "$paths_file" | git hash-object --stdin
)

tilt_fingerprint_matches() {
    local fingerprint_file="$1"
    local expected="$2"
    [[ -f "$fingerprint_file" ]] \
        && [[ "$(tr -d '\r\n' < "$fingerprint_file")" == "$expected" ]]
}

tilt_store_fingerprint() {
    local fingerprint_file="$1"
    local fingerprint="$2"
    local temporary="${fingerprint_file}.tmp.$$"

    printf '%s\n' "$fingerprint" > "$temporary"
    mv -f "$temporary" "$fingerprint_file"
}

tilt_acquire_lock() {
    local lock_dir="$1"
    local timeout_seconds="${2:-600}"
    local started_at now owner stale_dir

    mkdir -p "$(dirname "$lock_dir")"
    started_at="$(date +%s)"
    while ! mkdir "$lock_dir" 2>/dev/null; do
        owner="$(cat "$lock_dir/pid" 2>/dev/null || true)"
        if [[ "$owner" =~ ^[0-9]+$ ]] && ! kill -0 "$owner" 2>/dev/null; then
            stale_dir="${lock_dir}.stale.$$"
            if mv "$lock_dir" "$stale_dir" 2>/dev/null; then
                rm -f "$stale_dir/pid"
                rmdir "$stale_dir" 2>/dev/null || true
                continue
            fi
        fi

        now="$(date +%s)"
        if ((now - started_at >= timeout_seconds)); then
            echo "error: timed out waiting for Tilt build lock $lock_dir (owner ${owner:-unknown})" >&2
            return 1
        fi
        sleep 1
    done

    printf '%s\n' "$$" > "$lock_dir/pid"
    TILT_HELD_LOCK_DIR="$lock_dir"
}

tilt_release_lock() {
    local lock_dir="${TILT_HELD_LOCK_DIR:-}"
    [[ -n "$lock_dir" ]] || return 0

    local owner
    owner="$(cat "$lock_dir/pid" 2>/dev/null || true)"
    if [[ "$owner" == "$$" ]]; then
        rm -f "$lock_dir/pid"
        rmdir "$lock_dir" 2>/dev/null || true
    fi
    TILT_HELD_LOCK_DIR=""
}
