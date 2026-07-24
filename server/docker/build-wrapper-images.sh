#!/usr/bin/env bash
# ij6g: build the three catalog wrapper images and record their REAL immutable
# digests.
#
# Each image packages the stock service runtime plus the djinn protocol-v1
# control server. The digest written to the output manifest is the digest
# buildx reports for the pushed image — never a fabricated literal. The manifest
# is consumed by the djinn-image-controller wrapper_catalog reconciler
# (`reconcile_wrapper_catalog`) which records it into service_presets so strict
# canonical verification resolves `{wrapper_image}@{digest}`.
#
# Usage:
#   REGISTRY=ghcr.io/djinnos TAG=v0.6.130 ./server/docker/build-wrapper-images.sh
#
# Env:
#   REGISTRY   registry/org prefix for the wrapper repositories (required)
#   TAG        image tag to push (default: git short sha)
#   MANIFEST   output manifest path (default: ./wrapper-image-manifest.json)
#   PUSH       "1" to push (default 1); "0" builds locally without pushing/digest
set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd "$SCRIPT_DIR/../.." && pwd)

REGISTRY=${REGISTRY:?set REGISTRY (e.g. ghcr.io/djinnos)}
TAG=${TAG:-$(git -C "$REPO_ROOT" rev-parse --short HEAD)}
MANIFEST=${MANIFEST:-"$REPO_ROOT/wrapper-image-manifest.json"}
PUSH=${PUSH:-1}

# preset_id : wrapper repository (matches migration 149 seed) : bin : dockerfile
WRAPPERS="
preset-postgres-18|djinn-postgres-wrapper|djinn-postgres-wrapper|djinn-postgres-wrapper.Dockerfile
preset-redis-7|djinn-redis-wrapper|djinn-redis-wrapper|djinn-redis-wrapper.Dockerfile
preset-rabbitmq-4|djinn-rabbitmq-wrapper|djinn-rabbitmq-wrapper|djinn-rabbitmq-wrapper.Dockerfile
"

echo ">> building catalog wrapper binaries"
( cd "$REPO_ROOT/server" && cargo build --release --locked -p djinn-catalog-wrapper )

entries=""
for row in $WRAPPERS; do
    [ -n "$row" ] || continue
    preset_id=$(echo "$row" | cut -d'|' -f1)
    repo_name=$(echo "$row" | cut -d'|' -f2)
    bin=$(echo "$row" | cut -d'|' -f3)
    dockerfile=$(echo "$row" | cut -d'|' -f4)
    wrapper_image="$REGISTRY/$repo_name"
    ref="$wrapper_image:$TAG"

    echo ">> staging $bin"
    cp "$REPO_ROOT/server/target/release/$bin" "$SCRIPT_DIR/$bin"

    metafile=$(mktemp)
    if [ "$PUSH" = "1" ]; then
        echo ">> building + pushing $ref"
        docker buildx build \
            --file "$SCRIPT_DIR/$dockerfile" \
            --tag "$ref" \
            --push \
            --metadata-file "$metafile" \
            "$SCRIPT_DIR"
        digest=$(grep -o '"containerimage.digest"[^,}]*' "$metafile" \
            | grep -o 'sha256:[a-f0-9]\{64\}' || true)
        if [ -z "$digest" ]; then
            echo "ERROR: no digest reported for $ref" >&2
            exit 1
        fi
        entries="$entries{\"preset_id\":\"$preset_id\",\"wrapper_image\":\"$wrapper_image\",\"image_digest\":\"$digest\"},"
        echo "   $preset_id -> $wrapper_image@$digest"
    else
        echo ">> building (no push) $ref"
        docker build --file "$SCRIPT_DIR/$dockerfile" --tag "$ref" "$SCRIPT_DIR"
    fi
    rm -f "$metafile" "$SCRIPT_DIR/$bin"
done

if [ "$PUSH" = "1" ]; then
    printf '{"entries":[%s]}\n' "${entries%,}" > "$MANIFEST"
    echo ">> wrote wrapper image manifest: $MANIFEST"
fi
