#!/usr/bin/env bash
# seed-external-deps.sh — Hardlink external (non-workspace) dependency
# artifacts from the main worktree's target/ into the current worktree's
# target/, so that cargo only needs to compile workspace crates.
#
# IMPORTANT: Only seeds artifacts for external registry crates, NOT
# workspace crates (djinn-*).  Workspace crate artifacts may differ
# between branches and must be compiled fresh.
#
# Safety: Cargo.lock pins exact dependency versions, so external dep
# artifacts built on main are valid on any branch with the same lockfile.

set -euo pipefail

MAIN_TREE=$(cd "$(git rev-parse --git-common-dir)/.." && pwd)
MAIN_TARGET="${MAIN_TREE}/target"
LOCAL_TARGET="$(pwd)/target"

if [ ! -d "${MAIN_TARGET}/debug" ]; then
    echo "seed-external-deps: main target/debug/ not found, skipping"
    exit 0
fi

if [ "$(pwd)" = "${MAIN_TREE}" ]; then
    echo "seed-external-deps: running in main worktree, skipping"
    exit 0
fi

# Skip if local target already has a populated fingerprint dir (already seeded
# or previously built).
EXISTING=$( (find "${LOCAL_TARGET}/debug/.fingerprint" -mindepth 1 -maxdepth 1 -type d 2>/dev/null || true) | wc -l)
if [ "$EXISTING" -gt 10 ]; then
    echo "seed-external-deps: target already populated (${EXISTING} fingerprints), skipping"
    exit 0
fi

echo "seed-external-deps: seeding from ${MAIN_TARGET}"

# Pattern for workspace crate artifacts to EXCLUDE.
# Matches djinn-server, djinn-agent, djinn-core, djinn-db, djinn-git,
# djinn-mcp, djinn-provider, djinn_server, libdjinn_*, etc.
EXCLUDE_PATTERN="djinn[-_]"

SEED_DIRS=("debug/deps" "debug/.fingerprint" "debug/build")

for dir in "${SEED_DIRS[@]}"; do
    src="${MAIN_TARGET}/${dir}"
    dst="${LOCAL_TARGET}/${dir}"

    if [ ! -d "$src" ]; then
        continue
    fi

    mkdir -p "$dst"

    if [ "$dir" = "debug/deps" ]; then
        # Hardlink flat files, excluding workspace crate artifacts.
        find "$src" -maxdepth 1 -type f \
            ! -name "${EXCLUDE_PATTERN}*" \
            ! -name "lib${EXCLUDE_PATTERN}*" \
            -exec ln -f {} "$dst/" \; 2>/dev/null || true
    fi

    if [ "$dir" = "debug/.fingerprint" ]; then
        for fp_dir in "$src"/*/; do
            fp_name=$(basename "$fp_dir")
            case "$fp_name" in
                djinn[-_]*|libdjinn[-_]*) continue ;;
            esac
            if [ ! -d "$dst/$fp_name" ]; then
                mkdir -p "$dst/$fp_name"
            fi
            ln -f "$fp_dir"* "$dst/$fp_name/" 2>/dev/null || true
        done
    fi

    if [ "$dir" = "debug/build" ]; then
        for build_dir in "$src"/*/; do
            build_name=$(basename "$build_dir")
            case "$build_name" in
                djinn[-_]*|libdjinn[-_]*) continue ;;
            esac
            if [ ! -d "$dst/$build_name" ]; then
                cp -rl "$build_dir" "$dst/$build_name/../" 2>/dev/null || true
            fi
        done
    fi
done

# Seed top-level debug metadata.
for f in "${MAIN_TARGET}/debug/"*.d "${MAIN_TARGET}/debug/.cargo-lock"; do
    if [ -f "$f" ]; then
        ln -f "$f" "${LOCAL_TARGET}/debug/" 2>/dev/null || true
    fi
done

SEEDED=$( (find "${LOCAL_TARGET}/debug/.fingerprint" -mindepth 1 -maxdepth 1 -type d 2>/dev/null || true) | wc -l)
echo "seed-external-deps: seeded ${SEEDED} fingerprints"
