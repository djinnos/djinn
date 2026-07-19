#!/usr/bin/env bash
# Capture and validate the mold interface required by both build-image paths.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
EVIDENCE_DIR=""
while (($#)); do
    case "$1" in
        --evidence-dir) EVIDENCE_DIR="$2"; shift 2 ;;
        *) echo "usage: $0 --evidence-dir DIR" >&2; exit 2 ;;
    esac
done
[[ -n "$EVIDENCE_DIR" ]] || { echo "--evidence-dir is required" >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"

# The generated-image installer is the canonical package-pin declaration.
expected="$(sed -nE 's/^[[:space:]]*readonly[[:space:]]+MOLD_VERSION="([^"]+)".*/\1/p' \
    "$REPO_ROOT/server/crates/djinn-image-builder/scripts/install-rust.sh" | head -n1)"
[[ -n "$expected" ]] || { echo "could not read MOLD_VERSION pin" >&2; exit 1; }

mold --version > "$EVIDENCE_DIR/mold-version.txt" 2>&1
mold --help > "$EVIDENCE_DIR/mold-help.txt" 2>&1

# Debian's package revision (for example, +dfsg-1) is intentionally absent
# from mold --version. Preserve that raw upstream output above, and verify the
# package pin through dpkg rather than comparing two different version formats.
dpkg-query -W -f='${Version}\n' mold > "$EVIDENCE_DIR/mold-package-version.txt" 2>&1
if ! grep -Fxq "$expected" "$EVIDENCE_DIR/mold-package-version.txt"; then
    echo "installed mold package does not match pinned version $expected" >&2
    exit 1
fi

# mold 2.37 documents COUNT as the total number of threads. Require both the
# option spelling and its adjacent documented semantics; a similarly named,
# ambiguous flag must fail closed.
if ! grep -Eq -- '--threads=COUNT' "$EVIDENCE_DIR/mold-help.txt" \
        || ! grep -A2 -E -- '--threads=COUNT' "$EVIDENCE_DIR/mold-help.txt" \
            | grep -Fq 'Use COUNT number of threads'; then
    echo 'mold help lacks the required --threads=COUNT total-thread semantics' >&2
    exit 1
fi

printf 'validated mold %s\n' "$expected" | tee "$EVIDENCE_DIR/compatibility-result.txt"
