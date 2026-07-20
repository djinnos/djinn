#!/usr/bin/env bash
# Exercise the runtime-base mold apt stanza against the current trixie base.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
RUST_INSTALLER="$REPO_ROOT/server/crates/djinn-image-builder/scripts/install-rust.sh"
RUNTIME_DOCKERFILE="$REPO_ROOT/server/docker/djinn-agent-runtime-base.Dockerfile"

extract_value() {
    local file="$1"
    local name="$2"
    sed -nE "s/^[[:space:]]*(readonly[[:space:]]+|ARG[[:space:]]+)?${name}=\\\"?([^\\\"[:space:]]+)\\\"?.*/\\2/p" "$file" \
        | head -n1
}

snapshot="$(extract_value "$RUST_INSTALLER" DEBIAN_SNAPSHOT_URL)"
version="$(extract_value "$RUST_INSTALLER" MOLD_VERSION)"
runtime_snapshot="$(extract_value "$RUNTIME_DOCKERFILE" DEBIAN_SNAPSHOT_URL)"
runtime_version="$(extract_value "$RUNTIME_DOCKERFILE" MOLD_VERSION)"
[[ -n "$snapshot" && -n "$version" ]] || {
    echo 'could not read canonical mold snapshot and package version' >&2
    exit 1
}
[[ "$snapshot" == "$runtime_snapshot" && "$version" == "$runtime_version" ]] || {
    echo 'runtime-base mold snapshot or package version differs from generated-image pin' >&2
    exit 1
}

# Keep the raw probe evidence outside the container long enough to emit it on
# both success and failure. /var/tmp is available in CI and task sandboxes.
EVIDENCE_DIR="$(mktemp -d /var/tmp/djinn-mold-trixie-apt-smoke.XXXXXX)"
print_evidence() {
    local file
    for file in mold-version.txt mold-package-version.txt mold-help.txt compatibility-result.txt; do
        [[ -f "$EVIDENCE_DIR/$file" ]] || continue
        printf '\n===== %s =====\n' "$file"
        cat "$EVIDENCE_DIR/$file"
    done
}
cleanup() {
    local status=$?
    print_evidence
    rm -rf -- "$EVIDENCE_DIR"
    exit "$status"
}
trap cleanup EXIT

# This intentionally starts from the moving current-release base rather than
# building the full runtime image. The snapshot remains an additional source;
# checksums prove no pre-existing .list/.sources file was replaced.
docker run --rm \
    -e DEBIAN_SNAPSHOT_URL="$snapshot" \
    -e MOLD_VERSION="$version" \
    -v "$REPO_ROOT:/repo:ro" \
    -v "$EVIDENCE_DIR:/evidence" \
    debian:trixie-slim \
    bash -ceu '
        source_files() {
            find /etc/apt -type f \( -name "*.list" -o -name "*.sources" \) \
                ! -path /etc/apt/sources.list.d/mold-snapshot.list \
                -exec sha256sum {} + | sort
        }

        base_sources_before="$(source_files)"
        test -n "$base_sources_before"
        grep -Rqs trixie /etc/apt/sources.list /etc/apt/sources.list.d 2>/dev/null
        printf "deb [check-valid-until=no] %s trixie main\\n" "$DEBIAN_SNAPSHOT_URL" \
            > /etc/apt/sources.list.d/mold-snapshot.list
        test "$base_sources_before" = "$(source_files)"
        apt-get update
        apt-get install -y --no-install-recommends "mold=$MOLD_VERSION"
        /repo/scripts/image-ci/probe-mold-compatibility.sh --evidence-dir /evidence
    '
