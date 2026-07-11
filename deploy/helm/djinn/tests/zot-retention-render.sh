#!/usr/bin/env bash
# Helm rendering test for Zot catalog retention policy.
#
# Renders the zot-configmap template with retention enabled and disabled, then
# validates the extracted config.json structure with the Python standard
# library. Proves:
#   1. Enabled retention renders valid JSON with the catalog-only policy,
#      deleteUntagged, the configured newestTags count, and dry-run semantics.
#   2. Disabled retention renders valid JSON with no storage.retention block.
#   3. Required rendering and parsing tools cannot silently skip this test.
#
# Usage: bash deploy/helm/djinn/tests/zot-retention-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

require_tool() {
    if ! command -v "$1" &>/dev/null; then
        echo "FAIL: required test tool '$1' is not installed" >&2
        exit 1
    fi
}

require_tool helm
require_tool python3

TMPDIR_RENDER=$(mktemp -d)
trap 'rm -rf "$TMPDIR_RENDER"' EXIT

render_zot_configmap() {
    local output=$1
    shift

    helm template test-release "$CHART_DIR" \
        --show-only templates/zot-configmap.yaml \
        --set imagePipeline.enabled=true \
        --set imagePipeline.zot.enabled=true \
        "$@" \
        > "$output"
}

assert_config() {
    local render=$1
    local retention_enabled=$2
    local dry_run=${3:-unused}
    local newest_tags=${4:-unused}

    python3 - "$render" "$retention_enabled" "$dry_run" "$newest_tags" <<'PY'
import json
import sys

render_path, retention_enabled, expected_dry_run, expected_newest = sys.argv[1:]
rendered = open(render_path, encoding="utf-8").read().splitlines()

# Helm emits config.json as a YAML literal block. Extract that block directly
# so this focused test requires only Python's standard library, rather than an
# optionally-installed YAML package. Reject missing or malformed blocks.
try:
    config_start = rendered.index("  config.json: |") + 1
except ValueError:
    raise AssertionError("zot ConfigMap data.config.json was not rendered")

config_lines = []
for line in rendered[config_start:]:
    if line and not line.startswith("    "):
        break
    if line:
        config_lines.append(line[4:])
    else:
        config_lines.append("")

if not config_lines:
    raise AssertionError("zot ConfigMap data.config.json was empty")

config = json.loads("\n".join(config_lines))
storage = config.get("storage")
assert isinstance(storage, dict), "storage configuration missing"

if retention_enabled == "false":
    assert "retention" not in storage, "retention block present when disabled"
    print("PASS: disabled config.json parses and has no retention block")
    sys.exit(0)

retention = storage.get("retention")
assert isinstance(retention, dict), "storage.retention missing when enabled"
assert retention.get("dryRun") is (expected_dry_run == "true"), (
    f"dryRun should be {expected_dry_run}, got {retention.get('dryRun')}"
)

policies = retention.get("policies")
assert isinstance(policies, list) and len(policies) == 1, "expected exactly one retention policy"
policy = policies[0]
assert policy.get("repositories") == ["djinn-image-*"], (
    f"retention must target only catalog repositories, got {policy.get('repositories')}"
)
assert policy.get("deleteUntagged") is True, "deleteUntagged should be true"
keep_tags = policy.get("keepTags")
assert isinstance(keep_tags, list) and len(keep_tags) == 1, "expected exactly one keepTags rule"
assert keep_tags[0].get("newest") == int(expected_newest), (
    f"newestTags should be {expected_newest}, got {keep_tags[0].get('newest')}"
)

print(
    "PASS: enabled config.json parses with catalog-only selector, "
    f"deleteUntagged=true, newestTags={expected_newest}, dryRun={expected_dry_run}"
)
PY
}

echo "=== Test 1: retention disabled ==="
render_zot_configmap "$TMPDIR_RENDER/disabled.yaml" \
    --set imagePipeline.zot.retention.enabled=false
assert_config "$TMPDIR_RENDER/disabled.yaml" false

echo ""
echo "=== Test 2: retention enabled with dryRun=true ==="
render_zot_configmap "$TMPDIR_RENDER/enabled.yaml" \
    --set imagePipeline.zot.retention.enabled=true \
    --set imagePipeline.zot.retention.dryRun=true \
    --set imagePipeline.zot.retention.newestTags=3 \
    --set imagePipeline.zot.retention.deleteUntagged=true
assert_config "$TMPDIR_RENDER/enabled.yaml" true true 3

echo ""
echo "=== Test 3: retention enabled with dryRun=false (destructive) ==="
render_zot_configmap "$TMPDIR_RENDER/destructive.yaml" \
    --set imagePipeline.zot.retention.enabled=true \
    --set imagePipeline.zot.retention.dryRun=false \
    --set imagePipeline.zot.retention.newestTags=10 \
    --set imagePipeline.zot.retention.deleteUntagged=true
assert_config "$TMPDIR_RENDER/destructive.yaml" true false 10

echo ""
echo "=== All Helm rendering tests passed ==="
