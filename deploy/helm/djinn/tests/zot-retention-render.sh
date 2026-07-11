#!/usr/bin/env bash
# Helm rendering test for Zot catalog retention policy.
#
# Renders the zot-configmap template with retention enabled and disabled,
# then validates the config.json structure with python3 (available in the
# CI image). Proves:
#   1. With retention.enabled=true, the config.json includes a valid
#      storage.retention block with the djinn-image-* pattern, deleteUntagged,
#      and the configured newestTags count.
#   2. With retention.enabled=false (default), no retention block is rendered.
#   3. The retention policy never matches BuildKit/cache repos.
#
# Usage: bash deploy/helm/djinn/tests/zot-retention-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

if ! command -v helm &>/dev/null; then
    echo "SKIP: helm not installed" >&2
    exit 0
fi

TMPDIR_RENDER=$(mktemp -d)
trap 'rm -rf "$TMPDIR_RENDER"' EXIT

echo "=== Test 1: retention disabled (default) ==="
helm template test-release "$CHART_DIR" \
    --set imagePipeline.enabled=true \
    --set imagePipeline.zot.enabled=true \
    --set imagePipeline.zot.retention.enabled=false \
    > "$TMPDIR_RENDER/disabled.yaml" 2>&1

if grep -q '"retention"' "$TMPDIR_RENDER/disabled.yaml"; then
    echo "FAIL: retention block present when retention.enabled=false"
    exit 1
fi
echo "PASS: no retention block when disabled"

echo ""
echo "=== Test 2: retention enabled with dryRun=true ==="
helm template test-release "$CHART_DIR" \
    --set imagePipeline.enabled=true \
    --set imagePipeline.zot.enabled=true \
    --set imagePipeline.zot.retention.enabled=true \
    --set imagePipeline.zot.retention.dryRun=true \
    --set imagePipeline.zot.retention.newestTags=3 \
    --set imagePipeline.zot.retention.deleteUntagged=true \
    > "$TMPDIR_RENDER/enabled.yaml" 2>&1

# Extract the config.json from the ConfigMap.
CONFIG_JSON=$(python3 -c "
import yaml, sys, json
docs = list(yaml.safe_load_all(open('$TMPDIR_RENDER/enabled.yaml')))
for doc in docs:
    if doc and doc.get('kind') == 'ConfigMap' and 'zot-config' in doc.get('metadata', {}).get('name', ''):
        print(doc['data']['config.json'])
        sys.exit(0)
print('NOT FOUND', file=sys.stderr)
sys.exit(1)
")

echo "$CONFIG_JSON" | python3 -c "
import json, sys
config = json.load(sys.stdin)
retention = config.get('storage', {}).get('retention')
assert retention is not None, 'storage.retention missing when enabled'

assert retention['dryRun'] is True, f'dryRun should be true, got {retention[\"dryRun\"]}'
assert retention['policies'], 'no policies rendered'

policy = retention['policies'][0]
repos = policy['repositories']
assert 'djinn-image-*' in repos, f'djinn-image-* not in repos: {repos}'

# BuildKit/cache repos must NOT be in the policy.
for repo in repos:
    assert 'buildkit' not in repo.lower(), f'BuildKit repo in policy: {repo}'
    assert 'cache' not in repo.lower(), f'cache repo in policy: {repo}'

assert policy['deleteUntagged'] is True, f'deleteUntagged should be true'
keep_tags = policy['keepTags']
assert keep_tags[0]['newest'] == 3, f'newestTags should be 3, got {keep_tags[0][\"newest\"]}'

print('PASS: retention block renders correctly with djinn-image-*, deleteUntagged, newestTags=3')
"

echo ""
echo "=== Test 3: retention enabled with dryRun=false (destructive) ==="
helm template test-release "$CHART_DIR" \
    --set imagePipeline.enabled=true \
    --set imagePipeline.zot.enabled=true \
    --set imagePipeline.zot.retention.enabled=true \
    --set imagePipeline.zot.retention.dryRun=false \
    --set imagePipeline.zot.retention.newestTags=10 \
    > "$TMPDIR_RENDER/destructive.yaml" 2>&1

CONFIG_JSON=$(python3 -c "
import yaml, sys, json
docs = list(yaml.safe_load_all(open('$TMPDIR_RENDER/destructive.yaml')))
for doc in docs:
    if doc and doc.get('kind') == 'ConfigMap' and 'zot-config' in doc.get('metadata', {}).get('name', ''):
        print(doc['data']['config.json'])
        sys.exit(0)
print('NOT FOUND', file=sys.stderr)
sys.exit(1)
")

echo "$CONFIG_JSON" | python3 -c "
import json, sys
config = json.load(sys.stdin)
retention = config['storage']['retention']
assert retention['dryRun'] is False, 'dryRun should be false for destructive mode'
assert retention['policies'][0]['keepTags'][0]['newest'] == 10, 'newestTags should be 10'
print('PASS: destructive mode renders dryRun=false and newestTags=10')
"

echo ""
echo "=== All Helm rendering tests passed ==="
