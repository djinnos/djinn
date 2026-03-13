#!/usr/bin/env bash
# seed-build-cache.sh — Hardlink registry dependency artifacts from the main
# worktree's target/ into the current worktree's target/, so that `cargo test`
# only needs to compile workspace crates (djinn-server) instead of all 835+
# transitive dependencies.
#
# Usage: called as a setup command from inside an agent worktree.
# Assumes: main worktree has a populated target/debug/ from a prior build.

set -euo pipefail

MAIN_TREE=$(cd "$(git rev-parse --git-common-dir)/.." && pwd)
MAIN_TARGET="${MAIN_TREE}/target"
LOCAL_TARGET="$(pwd)/target"

if [ ! -d "${MAIN_TARGET}/debug" ]; then
    echo "seed-build-cache: main target/debug/ not found, skipping seed"
    exit 0
fi

# If we're in the main worktree, nothing to do
if [ "$(pwd)" = "${MAIN_TREE}" ]; then
    echo "seed-build-cache: running in main worktree, skipping"
    exit 0
fi

echo "seed-build-cache: seeding from ${MAIN_TARGET}"

# Directories to seed with hardlinks (immutable registry artifacts)
SEED_DIRS=("debug/deps" "debug/.fingerprint" "debug/build")

for dir in "${SEED_DIRS[@]}"; do
    src="${MAIN_TARGET}/${dir}"
    dst="${LOCAL_TARGET}/${dir}"

    if [ ! -d "$src" ]; then
        continue
    fi

    mkdir -p "$dst"

    # Hardlink everything EXCEPT workspace crate artifacts (djinn-server / djinn_server)
    # Using find + grep -v to exclude workspace artifacts, then ln to hardlink
    # We use -f to overwrite any existing files (e.g. from a previous seed)
    find "$src" -maxdepth 1 -type f \
        ! -name "djinn[-_]server*" \
        ! -name "libdjinn[-_]server*" \
        -exec ln -f {} "$dst/" \; 2>/dev/null || true

    # For .fingerprint, we need to hardlink directories (each fingerprint is a dir)
    if [ "$dir" = "debug/.fingerprint" ]; then
        # Remove the flat file links we just created (fingerprints are dirs, not files)
        find "$dst" -maxdepth 1 -type f -delete 2>/dev/null || true

        # Hardlink directory contents for non-workspace fingerprints
        for fp_dir in "$src"/*/; do
            fp_name=$(basename "$fp_dir")
            case "$fp_name" in
                djinn-server*|djinn_server*) continue ;;
            esac
            if [ ! -d "$dst/$fp_name" ]; then
                mkdir -p "$dst/$fp_name"
            fi
            ln -f "$fp_dir"* "$dst/$fp_name/" 2>/dev/null || true
        done
    fi

    # For build/, hardlink directory contents for non-workspace build scripts
    if [ "$dir" = "debug/build" ]; then
        find "$dst" -maxdepth 1 -type f -delete 2>/dev/null || true

        for build_dir in "$src"/*/; do
            build_name=$(basename "$build_dir")
            case "$build_name" in
                djinn-server*|djinn_server*) continue ;;
            esac
            if [ ! -d "$dst/$build_name" ]; then
                mkdir -p "$dst/$build_name"
            fi
            # Build dirs can have subdirectories (out/, root/, etc.)
            cp -rl "$build_dir" "$dst/$build_name/../" 2>/dev/null || true
        done
    fi
done

# Also seed the top-level debug directory metadata files
for f in "${MAIN_TARGET}/debug/"*.d "${MAIN_TARGET}/debug/.cargo-lock"; do
    if [ -f "$f" ]; then
        ln -f "$f" "${LOCAL_TARGET}/debug/" 2>/dev/null || true
    fi
done

SEEDED=$(find "${LOCAL_TARGET}/debug/.fingerprint" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
echo "seed-build-cache: seeded ${SEEDED} fingerprints"
