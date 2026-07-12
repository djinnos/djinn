#!/usr/bin/env bash
# Helm rendering test for Zot catalog retention policy and server preflight.
#
# Renders the Zot ConfigMap and server Deployment for disabled, dry-run, and
# destructive settings. Python's standard library verifies that the policy and
# the runtime startup-preflight environment stay coupled.
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

render_manifests() {
    local output=$1
    shift

    helm template test-release "$CHART_DIR" \
        --show-only templates/zot-configmap.yaml \
        --show-only templates/deployment-server.yaml \
        --set imagePipeline.enabled=true \
        --set imagePipeline.zot.enabled=true \
        "$@" \
        > "$output"
}

assert_manifests() {
    local render=$1
    local retention_enabled=$2
    local dry_run=$3
    local newest_tags=$4
    local expected_secret=$5

    python3 - "$render" "$retention_enabled" "$dry_run" "$newest_tags" "$expected_secret" <<'PY'
import json
import re
import sys

render_path, retention_enabled, expected_dry_run, expected_newest, expected_secret = sys.argv[1:]
rendered = open(render_path, encoding="utf-8").read().splitlines()

# Extract Zot's YAML literal block without requiring a third-party YAML parser.
try:
    config_start = rendered.index("  config.json: |") + 1
except ValueError:
    raise AssertionError("zot ConfigMap data.config.json was not rendered")

config_lines = []
for line in rendered[config_start:]:
    if line and not line.startswith("    "):
        break
    config_lines.append(line[4:] if line else "")
config = json.loads("\n".join(config_lines))
storage = config.get("storage")
assert isinstance(storage, dict), "storage configuration missing"

def env_block(name):
    start = next(i for i, line in enumerate(rendered)
                 if line == f"            - name: {name}")
    end = next((i for i in range(start + 1, len(rendered))
                if rendered[i].startswith("            - name: ")), len(rendered))
    return "\n".join(rendered[start:end])

def assert_value(name, expected):
    block = env_block(name)
    match = re.search(r"^              value: (.+)$", block, re.MULTILINE)
    assert match, f"{name} must use a literal non-secret value"
    assert match.group(1).strip('"') == expected, (
        f"{name} should be {expected}, got {match.group(1)}"
    )

# These values are the startup preflight's destructive-mode gate input and
# must exactly follow the rendered Zot policy settings in every scenario.
assert_value("DJINN_ZOT_RETENTION_ENABLED", retention_enabled)
assert_value("DJINN_ZOT_RETENTION_DRY_RUN", expected_dry_run)
assert_value("DJINN_ZOT_RETENTION_NEWEST_TAGS", expected_newest)
assert_value(
    "DJINN_ZOT_RETENTION_ENDPOINT",
    "http://test-release-djinn-zot.default.svc.cluster.local:5000",
)

for name, key in (("DJINN_ZOT_RETENTION_USERNAME", "username"),
                  ("DJINN_ZOT_RETENTION_PASSWORD", "password")):
    block = env_block(name)
    assert "valueFrom:" in block and "secretKeyRef:" in block, (
        f"{name} must use a SecretKeyRef"
    )
    assert f'name: "{expected_secret}"' in block, (
        f"{name} must reference {expected_secret}"
    )
    assert f"key: {key}" in block, f"{name} must use the {key} key"
    assert "value:" not in block, f"{name} must not render a secret literal"

if retention_enabled == "false":
    assert "retention" not in storage, "retention block present when disabled"
    print("PASS: disabled policy and non-destructive server preflight env agree")
    sys.exit(0)

retention = storage.get("retention")
assert isinstance(retention, dict), "storage.retention missing when enabled"
assert retention.get("dryRun") is (expected_dry_run == "true"), (
    f"dryRun should be {expected_dry_run}, got {retention.get('dryRun')}"
)
policy = retention.get("policies", [None])[0]
assert isinstance(policy, dict), "expected exactly one retention policy"
assert policy.get("repositories") == ["djinn-image-*"], "retention must target catalog repos only"
assert policy.get("deleteUntagged") is True, "deleteUntagged should be true"
assert policy.get("keepTags", [{}])[0].get("newest") == int(expected_newest), (
    f"newestTags should be {expected_newest}"
)
print(f"PASS: policy and server preflight env agree; newestTags={expected_newest}, dryRun={expected_dry_run}")
PY
}

echo "=== Test 1: retention disabled ==="
render_manifests "$TMPDIR_RENDER/disabled.yaml" \
    --set imagePipeline.zot.retention.enabled=false
assert_manifests "$TMPDIR_RENDER/disabled.yaml" false true 5 test-release-zot-auth

echo ""
echo "=== Test 2: retention enabled with dryRun=true ==="
render_manifests "$TMPDIR_RENDER/dry-run.yaml" \
    --set imagePipeline.zot.retention.enabled=true \
    --set imagePipeline.zot.retention.dryRun=true \
    --set imagePipeline.zot.retention.newestTags=3 \
    --set imagePipeline.zot.retention.deleteUntagged=true
assert_manifests "$TMPDIR_RENDER/dry-run.yaml" true true 3 test-release-zot-auth

echo ""
echo "=== Test 3: destructive retention ==="
render_manifests "$TMPDIR_RENDER/destructive.yaml" \
    --set imagePipeline.zot.retention.enabled=true \
    --set imagePipeline.zot.retention.dryRun=false \
    --set imagePipeline.zot.retention.newestTags=10 \
    --set imagePipeline.zot.retention.deleteUntagged=true
assert_manifests "$TMPDIR_RENDER/destructive.yaml" true false 10 test-release-zot-auth

echo ""
echo "=== Test 4: caller-owned existingSecret auth ==="
render_manifests "$TMPDIR_RENDER/existing-secret.yaml" \
    --set imagePipeline.zot.retention.enabled=true \
    --set imagePipeline.zot.retention.dryRun=true \
    --set imagePipeline.zot.auth.existingSecret=operator-zot-auth \
    --set imagePipeline.zot.auth.password=caller-secret-value
assert_manifests "$TMPDIR_RENDER/existing-secret.yaml" true true 5 operator-zot-auth
if grep -Fq 'caller-secret-value' "$TMPDIR_RENDER/existing-secret.yaml"; then
    echo "FAIL: caller-owned Zot password leaked into rendered manifests" >&2
    exit 1
fi

echo ""
echo "=== Test 5: destructive retention requires an enabled Zot endpoint ==="
if helm template test-release "$CHART_DIR" \
    --show-only templates/zot-configmap.yaml \
    --set imagePipeline.enabled=true \
    --set imagePipeline.zot.enabled=false \
    --set imagePipeline.zot.retention.enabled=true \
    --set imagePipeline.zot.retention.dryRun=false \
    > "$TMPDIR_RENDER/invalid.yaml" 2>&1; then
    echo "FAIL: destructive retention rendered without an enabled Zot endpoint" >&2
    exit 1
fi

echo ""
echo "=== All Helm rendering tests passed ==="
