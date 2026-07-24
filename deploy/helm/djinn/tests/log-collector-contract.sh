#!/usr/bin/env bash
# Hermetic collector contract: schema, rendered mount/security boundaries, and
# sanitization fixtures. Usage: bash deploy/helm/djinn/tests/log-collector-contract.sh
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES="$SCRIPT_DIR/fixtures/log-collector-sanitization.json"
command -v helm >/dev/null || { echo 'FAIL: helm is required' >&2; exit 1; }
command -v python3 >/dev/null || { echo 'FAIL: python3 is required' >&2; exit 1; }
command -v vector >/dev/null || { echo 'FAIL: vector is required to execute VRL fixtures' >&2; exit 1; }

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
python3 - "$TMPDIR_RENDER/enabled.yaml" "$FIXTURES" "$TMPDIR_RENDER" <<'PY'
import json, re, sys, textwrap
from pathlib import Path
manifest_path, fixtures_path, temporary = map(Path, sys.argv[1:])
text = manifest_path.read_text()
# Mount separation, Directory host paths, security and placement are all
# explicit textual contracts in this narrowly rendered manifest.
for expected in ('automountServiceAccountToken: false', 'mountPath: /source/pods',
                 'mountPath: /store', 'path: /var/log/pods', 'path: "/caller/retained"',
                 'type: Directory', 'runAsUser: 10002', 'runAsGroup: 10002',
                 'readOnlyRootFilesystem: true', 'allowPrivilegeEscalation: false',
                 'drop: ["ALL"]', 'workload-type: djinn', 'key: workload-type',
                 'uri: http://127.0.0.1:8687/ingest', 'retry_statuses: [507]',
                 'max_size: 67108864', 'when_full: drop_newest',
                 'for_each(json) -> |key, value| {', 'map_values(json, recursive: true)',
                 'original_bytes = length(value)', 'for_each(split(value, ""))',
                 'length(capped) + length(character) <= 2048', 'to_string(original_bytes)'):
    assert expected in text, expected
assert 'for_each(object:' not in text
assert 'strlen!' not in text
assert 'slice!(value, 0, 2048)' not in text
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
    def visit(item):
        if isinstance(item, dict):
            for key, value in item.items():
                norm = key.lower().replace('-', '_')
                if any(part in norm for part in sensitive): item[key] = '***REDACTED***'
                elif key in fields and isinstance(value, str): item[key] = cap(value)
            for value in item.values(): visit(value)
        elif isinstance(item, list):
            for value in item: visit(value)
    visit(obj)
    return json.dumps(obj, ensure_ascii=False, separators=(',', ':'))
runtime = []
for case in json.loads(fixtures_path.read_text()):
    value = case['input']
    if case.get('cap_field'):
        capped_value = case['cap_character'] * case['cap_count']
        payload = {case['cap_field']: capped_value}
        if case.get('cap_parent'): payload = {case['cap_parent']: payload}
        value = json.dumps(payload, ensure_ascii=False, separators=(',', ':'))
        result = sanitize(value)
        assert case['contains'] in result, case['name']
    else:
        result = sanitize(value)
        if 'equals' in case: assert result == case['equals'], (case['name'], result)
        else: assert case['contains'] in result, (case['name'], result)
    try:
        source_object = json.loads(value)
    except json.JSONDecodeError:
        source_object = None
    # Only messages originating as JSON objects get semantic comparison.
    # Every other input remains a byte-preservation contract.
    runtime.append((value, result, isinstance(source_object, dict)))

# Extract and execute the exact VRL shipped in the rendered ConfigMap.
sanitize_start = text.index('      sanitize:\n')
source_start = text.index('        source: |\n', sanitize_start) + len('        source: |\n')
source_end = text.index('\n      record:', source_start)
vrl = textwrap.dedent(text[source_start:source_end])
config = '''data_dir: {data_dir}
sources:
  fixture_input:
    type: stdin
    framing:
      method: character_delimited
      character_delimited:
        delimiter: "\u001e"
transforms:
  sanitize:
    type: remap
    inputs: [fixture_input]
    source: |
{vrl}sinks:
  fixture_output:
    type: console
    inputs: [sanitize]
    encoding:
      codec: json
'''.format(data_dir=temporary / 'vector-data', vrl=textwrap.indent(vrl, '      '))
(temporary / 'vector-fixtures.yaml').write_text(config)
(temporary / 'vector-input.txt').write_text(''.join(value + '\x1e' for value, _, _ in runtime))
(temporary / 'vector-expected.json').write_text(json.dumps([
    {'message': result, 'structural': structural}
    for _, result, structural in runtime
], ensure_ascii=False))
PY
vector --config "$TMPDIR_RENDER/vector-fixtures.yaml" <"$TMPDIR_RENDER/vector-input.txt" >"$TMPDIR_RENDER/vector-output.jsonl"
python3 - "$TMPDIR_RENDER/vector-output.jsonl" "$TMPDIR_RENDER/vector-expected.json" <<'PY'
import json, sys
actual = [json.loads(line)['message'] for line in open(sys.argv[1]) if line.strip()]
expected = json.load(open(sys.argv[2]))
assert len(actual) == len(expected), (actual, expected)
for index, (actual_message, expectation) in enumerate(zip(actual, expected)):
    expected_message = expectation['message']
    if expectation['structural']:
        try:
            actual_object = json.loads(actual_message)
            expected_object = json.loads(expected_message)
        except (json.JSONDecodeError, TypeError) as error:
            raise AssertionError((index, actual_message, expected_message)) from error
        assert isinstance(actual_object, dict), (index, actual_message, expected_message)
        assert actual_object == expected_object, (index, actual_message, expected_message)
    else:
        # Non-JSON messages are preservation contracts, so ordering tolerance must
        # never weaken their byte-for-byte comparison.
        assert actual_message == expected_message, (index, actual_message, expected_message)
print('PASS: rendered Vector VRL sanitization fixtures')
PY
