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
        --is-upgrade \
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
# The preflight endpoint must resolve to the namespace the Zot resources are
# actually rendered into. Derive that namespace from the render itself rather
# than hard-coding it, so a change to the chart's namespace default cannot make
# this assertion stale (it previously hard-coded "default" and silently rotted
# once values.namespace.name started defaulting to "djinn").
rendered_ns = next(
    (m.group(1) for m in (re.match(r"^  namespace: (\S+)$", line) for line in rendered) if m),
    None,
)
assert rendered_ns, "no rendered resource carried a metadata.namespace"
assert_value(
    "DJINN_ZOT_RETENTION_ENDPOINT",
    f"http://test-release-djinn-zot.{rendered_ns}.svc.cluster.local:5000",
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
assert_manifests "$TMPDIR_RENDER/disabled.yaml" false true 5 test-release-djinn-zot-auth

echo ""
echo "=== Test 2: retention enabled with dryRun=true ==="
render_manifests "$TMPDIR_RENDER/dry-run.yaml" \
    --set imagePipeline.zot.retention.enabled=true \
    --set imagePipeline.zot.retention.dryRun=true \
    --set imagePipeline.zot.retention.newestTags=3 \
    --set imagePipeline.zot.retention.deleteUntagged=true
assert_manifests "$TMPDIR_RENDER/dry-run.yaml" true true 3 test-release-djinn-zot-auth

echo ""
echo "=== Test 3: destructive retention ==="
render_manifests "$TMPDIR_RENDER/destructive.yaml" \
    --set imagePipeline.zot.retention.enabled=true \
    --set imagePipeline.zot.retention.dryRun=false \
    --set imagePipeline.zot.retention.newestTags=10 \
    --set imagePipeline.zot.retention.deleteUntagged=true
assert_manifests "$TMPDIR_RENDER/destructive.yaml" true false 10 test-release-djinn-zot-auth

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
echo "=== Test 6: shipped chart defaults are report-only, not unbounded growth ==="
# Every scenario above pins retention with an explicit --set, so none of them
# can observe what values.yaml actually ships. A cluster installed without any
# retention override is the common case and is what let one repo accumulate 117
# manifests / 83 GiB. Render with NO retention.* override at all and assert the
# shipped default renders a real, non-destructive policy.
render_manifests "$TMPDIR_RENDER/shipped-defaults.yaml"
assert_manifests "$TMPDIR_RENDER/shipped-defaults.yaml" true true 5 test-release-djinn-zot-auth

# assert_manifests reads the CATALOG policy out of the rendered JSON, but the
# safety glob deserves its own explicit, independent check: widening
# `repositories` beyond `djinn-image-*` would put the BuildKit registry cache
# repos (`cache/*` — see build_job.rs `--export-cache
# type=registry,ref=<reg>/cache/<subject>`) in scope of NEWEST-N TAG pruning,
# which is the wrong policy shape for them entirely. Assert the catalog glob is
# exactly the catalog one and that it is still the FIRST policy.
#
# `cache/*` has its own, differently shaped policy; it is asserted in
# tests/zot-cache-retention-render.sh. This test only guards the catalog one.
python3 - "$TMPDIR_RENDER/shipped-defaults.yaml" <<'PY'
import json, sys

rendered = open(sys.argv[1], encoding="utf-8").read().splitlines()
start = rendered.index("  config.json: |") + 1
lines = []
for line in rendered[start:]:
    if line and not line.startswith("    "):
        break
    lines.append(line[4:] if line else "")
config = json.loads("\n".join(lines))

policies = config["storage"]["retention"]["policies"]
catalog = [p for p in policies if p["repositories"] == ["djinn-image-*"]]
assert len(catalog) == 1, (
    f"expected exactly one ['djinn-image-*'] policy, got {len(catalog)} of {policies}"
)
assert policies[0] is catalog[0], (
    "the catalog policy must stay first; Zot applies the first matching policy "
    f"per repo, got {policies[0]['repositories']} first"
)
assert catalog[0]["deleteUntagged"] is True, "shipped default must delete untagged manifests"
# No OTHER policy may claim a glob that could also match a catalog repo.
for other in policies[1:]:
    for glob in other["repositories"]:
        assert not glob.startswith("djinn-image"), (
            f"a non-catalog policy widened into catalog scope: {glob}"
        )
assert config["storage"]["retention"]["dryRun"] is True, (
    "shipped default must be dry-run: destructive deletion is an explicit operator opt-in"
)
print("PASS: shipped defaults render a report-only catalog policy scoped to djinn-image-* only")
PY

echo ""
echo "=== Test 7: default retention stays inert without an in-cluster Zot ==="
# The startup preflight is fail-closed: on a Zot fetch error it returns Err and
# the leader calls std::process::exit(1). External-registry deployments (the
# chart default imagePipeline.zot.enabled=false) render no Zot ConfigMap and no
# auth Secret keys, so telling the server retention is enabled would crash-loop
# it on boot. Assert the shipped `retention.enabled: true` default is ANDed down
# to an effective "false" whenever no in-cluster Zot is rendered.
assert_effective_disabled() {
    local render=$1
    local label=$2
    python3 - "$render" "$label" <<'PY'
import re, sys

render_path, label = sys.argv[1:]
rendered = open(render_path, encoding="utf-8").read().splitlines()

start = next(i for i, line in enumerate(rendered)
             if line == "            - name: DJINN_ZOT_RETENTION_ENABLED")
end = next((i for i in range(start + 1, len(rendered))
            if rendered[i].startswith("            - name: ")), len(rendered))
block = "\n".join(rendered[start:end])
match = re.search(r"^              value: (.+)$", block, re.MULTILINE)
assert match, "DJINN_ZOT_RETENTION_ENABLED must render a literal value"
value = match.group(1).strip('"')
assert value == "false", (
    f"{label}: effective retention must be false when no in-cluster Zot is "
    f"rendered, or the fail-closed startup preflight exit(1)s the server; got {value}"
)
# A preflight that cannot authenticate is exactly the fetch error that kills
# boot, so the credential refs must be absent in this topology too.
assert "DJINN_ZOT_RETENTION_USERNAME" not in "\n".join(rendered), (
    f"{label}: Zot auth SecretKeyRefs must not render without in-cluster Zot"
)
print(f"PASS: {label} renders effective retention=false")
PY
}

# 7a: chart defaults (imagePipeline.enabled=true, zot.enabled=false).
helm template test-release "$CHART_DIR" \
    --is-upgrade \
    --show-only templates/deployment-server.yaml \
    > "$TMPDIR_RENDER/no-zot.yaml"
assert_effective_disabled "$TMPDIR_RENDER/no-zot.yaml" "external-registry defaults"

# 7b: retention explicitly requested but the whole image pipeline is off.
helm template test-release "$CHART_DIR" \
    --is-upgrade \
    --show-only templates/deployment-server.yaml \
    --set imagePipeline.enabled=false \
    --set imagePipeline.zot.retention.enabled=true \
    > "$TMPDIR_RENDER/no-pipeline.yaml"
assert_effective_disabled "$TMPDIR_RENDER/no-pipeline.yaml" "image pipeline disabled"

echo ""
echo "=== All Helm rendering tests passed ==="
