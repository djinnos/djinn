#!/usr/bin/env bash
# ij6g: exact Helm render contract for the catalog wrapper image manifest.
#
# Two properties are load-bearing and are asserted here:
#   1. Unset `serviceWrappers.imageManifest` renders NO ConfigMap, NO volume
#      and NO DJINN_WRAPPER_IMAGE_MANIFEST — strict service verification then
#      stays fail-closed rather than resolving a wrapper with no verified
#      digest.
#   2. A configured manifest reaches the server container BYTE-FOR-BYTE as
#      JSON. The digests in it are the registry's, so any mangling here would
#      either fail the identity validation in `set_wrapper_identity` or, worse,
#      record a wrapper image nobody can pull.
#
# Usage: bash deploy/helm/djinn/tests/wrapper-manifest-render.sh
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
require_tool() { command -v "$1" >/dev/null 2>&1 || { echo "FAIL: required test tool '$1' is not installed" >&2; exit 1; }; }
require_tool helm
require_tool python3
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

MOUNT_PATH="/var/run/djinn/wrapper/wrapper-image-manifest.json"
DIGEST_PG="sha256:$(printf 'a%.0s' $(seq 64))"
DIGEST_REDIS="sha256:$(printf 'b%.0s' $(seq 64))"

echo "=== unset: no ConfigMap, no env, no volume ==="
helm template wrapper-test "$CHART_DIR" \
    --set-string migration.designatedOperatorSecret=wrapper-test-operator \
    > "$TMP/none.yaml"
for needle in "DJINN_WRAPPER_IMAGE_MANIFEST" "wrapper-image-manifest.json" "name: wrapper-manifest"; do
    if grep -q -- "$needle" "$TMP/none.yaml"; then
        echo "FAIL: default render must not contain '$needle'" >&2
        exit 1
    fi
done

echo "=== configured (YAML mapping): exact JSON reaches the server ==="
cat > "$TMP/values-map.yaml" <<EOF
migration:
  designatedOperatorSecret: wrapper-test-operator
serviceWrappers:
  imageManifest:
    entries:
      - preset_id: preset-postgres-18
        wrapper_image: ghcr.io/djinnos/djinn-postgres-wrapper
        image_digest: "$DIGEST_PG"
      - preset_id: preset-redis-7
        wrapper_image: ghcr.io/djinnos/djinn-redis-wrapper
        image_digest: "$DIGEST_REDIS"
EOF
helm template wrapper-test "$CHART_DIR" -f "$TMP/values-map.yaml" > "$TMP/map.yaml"

echo "=== configured (JSON string): identical projection ==="
cat > "$TMP/values-str.yaml" <<EOF
migration:
  designatedOperatorSecret: wrapper-test-operator
serviceWrappers:
  imageManifest: |
    {"entries":[{"preset_id":"preset-postgres-18","wrapper_image":"ghcr.io/djinnos/djinn-postgres-wrapper","image_digest":"$DIGEST_PG"},{"preset_id":"preset-redis-7","wrapper_image":"ghcr.io/djinnos/djinn-redis-wrapper","image_digest":"$DIGEST_REDIS"}]}
EOF
helm template wrapper-test "$CHART_DIR" -f "$TMP/values-str.yaml" > "$TMP/str.yaml"

MOUNT_PATH="$MOUNT_PATH" DIGEST_PG="$DIGEST_PG" DIGEST_REDIS="$DIGEST_REDIS" \
python3 - "$TMP/map.yaml" "$TMP/str.yaml" <<'PY'
import json
import os
import sys

try:
    import yaml
except ImportError:  # PyYAML is not guaranteed on every runner
    yaml = None

mount_path = os.environ["MOUNT_PATH"]
expected = {
    "entries": [
        {
            "preset_id": "preset-postgres-18",
            "wrapper_image": "ghcr.io/djinnos/djinn-postgres-wrapper",
            "image_digest": os.environ["DIGEST_PG"],
        },
        {
            "preset_id": "preset-redis-7",
            "wrapper_image": "ghcr.io/djinnos/djinn-redis-wrapper",
            "image_digest": os.environ["DIGEST_REDIS"],
        },
    ]
}


def projected_json(path):
    """The single `wrapper-image-manifest.json:` ConfigMap value, decoded."""
    values = []
    for line in open(path, encoding="utf-8").read().splitlines():
        stripped = line.strip()
        if stripped.startswith("wrapper-image-manifest.json:"):
            raw = stripped.split(":", 1)[1].strip()
            assert raw.startswith('"'), f"manifest must render as a quoted scalar, got {raw[:40]}"
            values.append(json.loads(raw) if yaml is None else yaml.safe_load(raw))
    assert len(values) == 1, f"expected exactly one manifest entry in {path}, got {len(values)}"
    return json.loads(values[0])


def asserts_common(path):
    text = open(path, encoding="utf-8").read()
    assert projected_json(path) == expected, f"{path}: manifest content differs from the values"
    assert f"value: {mount_path}" in text, f"{path}: env must point at {mount_path}"
    assert "mountPath: /var/run/djinn/wrapper" in text, f"{path}: manifest volume is not mounted"
    assert "name: wrapper-test-djinn-wrapper-manifest" in text, f"{path}: ConfigMap is not referenced"


for manifest_path in sys.argv[1:]:
    asserts_common(manifest_path)

print("OK: wrapper manifest render contract holds for mapping and string forms")
PY
