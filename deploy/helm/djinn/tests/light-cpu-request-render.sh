#!/usr/bin/env bash
# Render contract for the role-aware light CPU request sent to djinn-server.
#
# The generated setting must be overridable through values and must precede
# extraEnv: Kubernetes uses the last duplicate environment entry, so an
# operator-provided duplicate remains the effective value.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

require_tool() {
    command -v "$1" >/dev/null 2>&1 || {
        printf "FAIL: required test tool '%s' is not installed\n" "$1" >&2
        exit 1
    }
}

require_tool helm
require_tool python3

WORK=$(mktemp -d "${TMPDIR:-/var/tmp}/djinn-light-cpu.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

render() {
    local chart=$1
    local output=$2
    shift 2
    helm template light-cpu-request-test "$chart" --is-upgrade \
        --show-only templates/deployment-server.yaml "$@" >"$output"
}

# Inspect the parsed Deployment env array rather than source lines or an
# absolute array position. Diagnostics include every rendered name/value/index
# so a broken template can be diagnosed directly from the contract output.
assert_light_cpu_value() {
    local manifest=$1
    local expected_value=$2
    python3 - "$manifest" "$expected_value" <<'PY'
import json
import re
import sys

manifest, expected = sys.argv[1:]


def rendered_server_env(path):
    """Extract this container's env sequence using YAML indentation boundaries."""
    lines = open(path, encoding="utf-8").read().splitlines()
    documents, current = [], []
    for line in lines:
        if line == "---":
            documents.append(current)
            current = []
        else:
            current.append(line)
    documents.append(current)
    deployments = [doc for doc in documents if any(line == "kind: Deployment" for line in doc)]
    assert len(deployments) == 1, f"expected one rendered Deployment, found {len(deployments)}"
    doc = deployments[0]
    starts = [i for i, line in enumerate(doc) if re.match(r"^(\s*)- name: djinn-server\s*$", line)]
    assert len(starts) == 1, f"expected one djinn-server container, found {len(starts)}"
    start = starts[0]
    container_indent = len(doc[start]) - len(doc[start].lstrip())
    end = next((i for i in range(start + 1, len(doc)) if re.match(rf"^ {{{container_indent}}}- name:", doc[i])), len(doc))
    env_lines = doc[start:end]
    env_headers = [i for i, line in enumerate(env_lines) if re.match(r"^(\s*)env:\s*$", line)]
    assert len(env_headers) == 1, f"expected one djinn-server env list, found {len(env_headers)}"
    env_indent = len(env_lines[env_headers[0]]) - len(env_lines[env_headers[0]].lstrip())
    entries, entry_indent = [], env_indent + 2
    for i, line in enumerate(env_lines[env_headers[0] + 1:], env_headers[0] + 1):
        if line.strip() and len(line) - len(line.lstrip()) <= env_indent:
            break
        match = re.match(rf"^ {{{entry_indent}}}- name: (.+)$", line)
        if not match:
            continue
        name, value = match.group(1).strip().strip('"'), "<valueFrom>"
        for candidate in env_lines[i + 1:]:
            if re.match(rf"^ {{{entry_indent}}}- name: ", candidate) or (candidate.strip() and len(candidate) - len(candidate.lstrip()) <= env_indent):
                break
            value_match = re.match(rf"^ {{{entry_indent + 2}}}value: (.+)$", candidate)
            if value_match:
                raw_value = value_match.group(1).strip()
                value = json.loads(raw_value) if raw_value.startswith('"') else raw_value
                break
        entries.append((len(entries), name, value))
    return entries


entries = rendered_server_env(manifest)
matches = [(index, value) for index, name, value in entries if name == "DJINN_K8S_LIGHT_CPU_REQUEST"]
assert len(matches) == 1, (
    "DJINN_K8S_LIGHT_CPU_REQUEST must be present exactly once; "
    f"matches={matches}; rendered env indices/names/values={entries}"
)
index, value = matches[0]
assert value == expected, (
    "DJINN_K8S_LIGHT_CPU_REQUEST has the wrong rendered value; "
    f"expected={expected!r}; actual={value!r}; index={index}; "
    f"rendered env indices/names/values={entries}"
)
PY
}

assert_extra_env_order() {
    local manifest=$1
    local expected_generated_value=$2
    python3 - "$manifest" "$expected_generated_value" <<'PY'
import json
import re
import sys

manifest, expected = sys.argv[1:]


def rendered_server_env(path):
    """Extract this container's env sequence using YAML indentation boundaries."""
    lines = open(path, encoding="utf-8").read().splitlines()
    documents, current = [], []
    for line in lines:
        if line == "---":
            documents.append(current)
            current = []
        else:
            current.append(line)
    documents.append(current)
    deployments = [doc for doc in documents if any(line == "kind: Deployment" for line in doc)]
    assert len(deployments) == 1, f"expected one rendered Deployment, found {len(deployments)}"
    doc = deployments[0]
    starts = [i for i, line in enumerate(doc) if re.match(r"^(\s*)- name: djinn-server\s*$", line)]
    assert len(starts) == 1, f"expected one djinn-server container, found {len(starts)}"
    start = starts[0]
    container_indent = len(doc[start]) - len(doc[start].lstrip())
    end = next((i for i in range(start + 1, len(doc)) if re.match(rf"^ {{{container_indent}}}- name:", doc[i])), len(doc))
    env_lines = doc[start:end]
    env_headers = [i for i, line in enumerate(env_lines) if re.match(r"^(\s*)env:\s*$", line)]
    assert len(env_headers) == 1, f"expected one djinn-server env list, found {len(env_headers)}"
    env_indent = len(env_lines[env_headers[0]]) - len(env_lines[env_headers[0]].lstrip())
    entries, entry_indent = [], env_indent + 2
    for i, line in enumerate(env_lines[env_headers[0] + 1:], env_headers[0] + 1):
        if line.strip() and len(line) - len(line.lstrip()) <= env_indent:
            break
        match = re.match(rf"^ {{{entry_indent}}}- name: (.+)$", line)
        if not match:
            continue
        name, value = match.group(1).strip().strip('"'), "<valueFrom>"
        for candidate in env_lines[i + 1:]:
            if re.match(rf"^ {{{entry_indent}}}- name: ", candidate) or (candidate.strip() and len(candidate) - len(candidate.lstrip()) <= env_indent):
                break
            value_match = re.match(rf"^ {{{entry_indent + 2}}}value: (.+)$", candidate)
            if value_match:
                raw_value = value_match.group(1).strip()
                value = json.loads(raw_value) if raw_value.startswith('"') else raw_value
                break
        entries.append((len(entries), name, value))
    return entries


entries = rendered_server_env(manifest)

def indices(name, value):
    return [index for index, entry_name, entry_value in entries if entry_name == name and entry_value == value]

generated = indices("DJINN_K8S_LIGHT_CPU_REQUEST", expected)
operator_duplicate = indices("DJINN_K8S_LIGHT_CPU_REQUEST", "operator-wins")
operator_marker = indices("DJINN_LIGHT_CPU_ORDER_MARKER", "after-generated")
assert len(generated) == 1, (
    "expected exactly one generated light CPU env entry; "
    f"generated_indices={generated}; rendered env indices/names/values={entries}"
)
assert len(operator_duplicate) == 1 and len(operator_marker) == 1, (
    "extraEnv duplicate/marker was not rendered exactly once; "
    f"duplicate_indices={operator_duplicate}; marker_indices={operator_marker}; "
    f"rendered env indices/names/values={entries}"
)
assert generated[0] < operator_duplicate[0] and generated[0] < operator_marker[0], (
    "generated light CPU entry must precede extraEnv entries; "
    f"generated_index={generated[0]}; duplicate_index={operator_duplicate[0]}; "
    f"marker_index={operator_marker[0]}; rendered env indices/names/values={entries}"
)
PY
}

expect_assertion_failure() {
    local mutation=$1
    shift
    local output
    if output=$("$@" 2>&1); then
        printf 'FAIL: %s mutation unexpectedly passed its relevant assertion\n' "$mutation" >&2
        exit 1
    fi
    printf 'expected failure for %s mutation:\n%s\n' "$mutation" "$output"
}

copy_chart() {
    local destination=$1
    cp -R "$CHART_DIR" "$destination"
}

# Baseline and value override: each stock/override render has exactly one
# generated setting before any duplicate is intentionally supplied.
render "$CHART_DIR" "$WORK/stock.yaml"
assert_light_cpu_value "$WORK/stock.yaml" "300m"
render "$CHART_DIR" "$WORK/override.yaml" --set-string resources.taskrun.requests.lightCpu=450m
assert_light_cpu_value "$WORK/override.yaml" "450m"

extra_env_json='[{"name":"DJINN_K8S_LIGHT_CPU_REQUEST","value":"operator-wins"},{"name":"DJINN_LIGHT_CPU_ORDER_MARKER","value":"after-generated"}]'
render "$CHART_DIR" "$WORK/order.yaml" --set-json "extraEnv=$extra_env_json"
assert_extra_env_order "$WORK/order.yaml" "300m"

# Mutation checks operate only on copies. They prove these assertions fail for
# the intended defect instead of merely passing against the current template.
missing_chart="$WORK/missing-chart"
copy_chart "$missing_chart"
python3 - "$missing_chart/templates/deployment-server.yaml" <<'PY'
import sys
path = sys.argv[1]
text = open(path, encoding="utf-8").read()
stanza = '''            - name: DJINN_K8S_LIGHT_CPU_REQUEST
              value: {{ .Values.resources.taskrun.requests.lightCpu | quote }}
'''
assert text.count(stanza) == 1, "mutation seam for generated light CPU stanza was not unique"
open(path, "w", encoding="utf-8").write(text.replace(stanza, ""))
PY
render "$missing_chart" "$WORK/missing.yaml"
expect_assertion_failure "deleted generated env (absent)" assert_light_cpu_value "$WORK/missing.yaml" "300m"

hard_coded_chart="$WORK/hard-coded-chart"
copy_chart "$hard_coded_chart"
python3 - "$hard_coded_chart/templates/deployment-server.yaml" <<'PY'
import sys
path = sys.argv[1]
text = open(path, encoding="utf-8").read()
needle = '.Values.resources.taskrun.requests.lightCpu | quote'
assert text.count(needle) == 1, "mutation seam for light CPU value was not unique"
open(path, "w", encoding="utf-8").write(text.replace(needle, '"300m"'))
PY
render "$hard_coded_chart" "$WORK/hard-coded.yaml" --set-string resources.taskrun.requests.lightCpu=450m
expect_assertion_failure "hard-coded 300m override" assert_light_cpu_value "$WORK/hard-coded.yaml" "450m"

reordered_chart="$WORK/reordered-chart"
copy_chart "$reordered_chart"
python3 - "$reordered_chart/templates/deployment-server.yaml" <<'PY'
import sys
path = sys.argv[1]
text = open(path, encoding="utf-8").read()
stanza = '''            - name: DJINN_K8S_LIGHT_CPU_REQUEST
              value: {{ .Values.resources.taskrun.requests.lightCpu | quote }}
'''
extra_env = '''            {{- with .Values.extraEnv }}
            {{- toYaml . | nindent 12 }}
            {{- end }}
'''
assert text.count(stanza) == 1, "mutation seam for generated light CPU stanza was not unique"
assert text.count(extra_env) == 1, "mutation seam for final extraEnv include was not unique"
text = text.replace(stanza, "")
open(path, "w", encoding="utf-8").write(text.replace(extra_env, extra_env + stanza))
PY
render "$reordered_chart" "$WORK/reordered.yaml" --set-json "extraEnv=$extra_env_json"
expect_assertion_failure "generated env moved below extraEnv" assert_extra_env_order "$WORK/reordered.yaml" "300m"

printf '=== light CPU request Helm render contract passed ===\n'
