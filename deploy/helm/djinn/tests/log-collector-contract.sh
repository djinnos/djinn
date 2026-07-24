#!/usr/bin/env bash
# Hermetic collector contract: schema, rendered mount/security boundaries, and
# sanitization fixtures. Usage: bash deploy/helm/djinn/tests/log-collector-contract.sh
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES="$SCRIPT_DIR/fixtures/log-collector-sanitization.json"
command -v helm >/dev/null || { echo 'FAIL: helm is required' >&2; exit 1; }
command -v python3 >/dev/null || { echo 'FAIL: python3 is required' >&2; exit 1; }

TMPDIR_RENDER=$(mktemp -d)
trap 'rm -rf "$TMPDIR_RENDER"' EXIT
# Defaults must remain inert.
helm template test "$CHART_DIR" >"$TMPDIR_RENDER/default.yaml"
! grep -q 'log-collector' "$TMPDIR_RENDER/default.yaml"
# Schema rejects mutable tags and capacities outside the bounded 64 MiB contract.
for bad in 'logCollector.rotatorImage=example/rotator:latest' 'logCollector.vectorImage=example/vector:latest' 'logCollector.bufferMiB=65' 'logCollector.storePath=relative'; do
  if helm template test "$CHART_DIR" --set logCollector.enabled=true --set "$bad" >/dev/null 2>&1; then
    echo "FAIL: schema accepted $bad" >&2; exit 1
  fi
done
helm template test "$CHART_DIR" --set logCollector.enabled=true \
  --set logCollector.rotatorImage=example/rotator:1.2.3 \
  --set logCollector.vectorImage=example/vector:0.43.1 \
  --set logCollector.storePath=/caller/retained \
  --set logCollector.bufferMiB=64 \
  --set 'resources.taskrun.nodeSelector.workload-type=djinn' \
  --set 'resources.taskrun.tolerations[0].key=workload-type' \
  --set 'resources.taskrun.tolerations[0].operator=Exists' \
  --show-only templates/daemonset-log-collector.yaml \
  --show-only templates/configmap-log-collector.yaml >"$TMPDIR_RENDER/enabled.yaml"
python3 - "$TMPDIR_RENDER/enabled.yaml" "$FIXTURES" <<'PY'
import json, re, sys
manifest, fixtures = map(open, sys.argv[1:])
text = manifest.read()
# Mount separation, Directory host paths, security and placement are all
# explicit textual contracts in this narrowly rendered manifest.
for expected in ('automountServiceAccountToken: false', 'mountPath: /source/pods',
                 'mountPath: /store', 'path: /var/log/pods', 'path: "/caller/retained"',
                 'type: Directory', 'runAsUser: 10002', 'runAsGroup: 10002',
                 'readOnlyRootFilesystem: true', 'allowPrivilegeEscalation: false',
                 'drop: ["ALL"]', 'workload-type: djinn', 'key: workload-type',
                 'uri: http://127.0.0.1:8687/ingest', 'retry_statuses: [507]',
                 'max_size: 67108864', 'when_full: drop_newest'):
    assert expected in text, expected
vector = text[text.index('- name: vector'):text.index('- name: rotator')]
rotator = text[text.index('- name: rotator'):text.index('volumes:')]
assert '/source/pods' in vector and '/store' not in vector
assert '/store' in rotator and '/source/pods' not in rotator
assert 'readOnly: true' in vector
assert 'readOnly: true' not in rotator
# Reference policy mirrors infra_death_log_tail: parse only one-line objects,
# redact case-insensitive sensitive keys/env assignments, and cap only six
# JSON string fields at a UTF-8 boundary with original byte accounting.
sensitive = ('authorization','apikey','api_key','access_token','secret','bearer','password','passwd')
fields = {'statement','sql','query','request_body','response_body','body'}
env = re.compile(r'(?i)^(\s*(?:API_KEY|APIKEY|SECRET|TOKEN|PASSWORD|PASSWD|AUTH|BEARER|OPENAI_API_KEY|ANTHROPIC_API_KEY|GITHUB_TOKEN|GH_TOKEN|AWS_SECRET|AWS_SESSION_TOKEN)(?:_[A-Z0-9_]+)?=).*$')
def cap(value):
    raw = value.encode()
    if len(raw) <= 2048: return value
    cut = raw[:2048]
    while True:
        try: prefix = cut.decode(); break
        except UnicodeDecodeError: cut = cut[:-1]
    return f'{prefix}…[FIELD_TRUNCATED original_bytes={len(raw)}]'
def sanitize(value):
    if '\n' in value or '\r' in value or 'djinn.panic_summary.v1' in value:
        return value
    try: obj = json.loads(value)
    except json.JSONDecodeError: return env.sub(r'\1***REDACTED***', value)
    if not isinstance(obj, dict): return value
    for key, item in obj.items():
        norm = key.lower().replace('-', '_')
        if any(part in norm for part in sensitive): obj[key] = '***REDACTED***'
        elif key in fields and isinstance(item, str): obj[key] = cap(item)
    return json.dumps(obj, ensure_ascii=False, separators=(',', ':'))
for case in json.load(fixtures):
    value = case['input']
    if case.get('cap_field'):
        value = json.dumps({case['cap_field']: 'é' * 1100}, ensure_ascii=False, separators=(',', ':'))
        result = sanitize(value)
        assert '…[FIELD_TRUNCATED original_bytes=2200]' in result, case['name']
    else:
        result = sanitize(value)
        if 'equals' in case: assert result == case['equals'], (case['name'], result)
        else: assert case['contains'] in result, (case['name'], result)
print('PASS: log collector render and sanitization contract')
PY
