#!/usr/bin/env bash
# Build the fbiy-C1 probe image and load it onto a disposable kind node.
#
#   build.sh <image-tag> [kind-cluster-name]
#
# Two `cargo build --release` invocations and one `docker build`. The binaries
# are built OUT of this repository so the launcher under test is the shipped one
# — `server/crates/djinn-cgroup-launcher/src/main.rs`, unmodified — rather than a
# copy of it packaged for the test.
#
# `kind load docker-image` rather than a push to the harness registry: a pull
# failure would surface as "the Pod never ran", which is indistinguishable from
# the launcher readiness failure this harness exists to detect.
set -euo pipefail

IMAGE=${1:?usage: build.sh <image-tag> [kind-cluster-name]}
CLUSTER=${2:-}

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# governor-probe -> fixtures -> tests -> djinn-k8s -> crates -> server -> repo
REPO_ROOT="$(cd "$HERE/../../../../../.." && pwd)"
SERVER_DIR="$REPO_ROOT/server"

[ -d "$SERVER_DIR/crates/djinn-cgroup-launcher" ] \
    || { printf 'FAIL: %s is not the djinn server workspace\n' "$SERVER_DIR" >&2; exit 2; }

printf '>>> building djinn-cgroup-launcher and governor_probe (release)\n'
(
    cd "$SERVER_DIR"
    cargo build --release -p djinn-cgroup-launcher \
        --bin djinn-cgroup-launcher --example governor_probe
)

TARGET="$SERVER_DIR/target/release"
CONTEXT="$(mktemp -d)"
trap 'rm -rf "$CONTEXT"' EXIT

cp "$TARGET/djinn-cgroup-launcher" "$CONTEXT/djinn-cgroup-launcher"
cp "$TARGET/examples/governor_probe" "$CONTEXT/governor_probe"
cp "$HERE/Dockerfile" "$CONTEXT/Dockerfile"

printf '>>> docker build %s\n' "$IMAGE"
docker build --quiet --tag "$IMAGE" "$CONTEXT" >/dev/null

if [ -n "$CLUSTER" ]; then
    printf '>>> kind load docker-image %s --name %s\n' "$IMAGE" "$CLUSTER"
    kind load docker-image "$IMAGE" --name "$CLUSTER"
fi

printf 'PASS: %s is built%s\n' "$IMAGE" "${CLUSTER:+ and loaded onto $CLUSTER}"
