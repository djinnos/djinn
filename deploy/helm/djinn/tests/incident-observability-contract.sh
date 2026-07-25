#!/usr/bin/env bash
# Hermetic chart contract for incident observability. Usage:
# bash deploy/helm/djinn/tests/incident-observability-contract.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES="$SCRIPT_DIR/fixtures"
command -v helm >/dev/null || { echo 'FAIL: helm is required' >&2; exit 1; }
command -v python3 >/dev/null || { echo 'FAIL: python3 is required' >&2; exit 1; }

render_dir=$(mktemp -d)
trap 'rm -rf "$render_dir"' EXIT

# The collector owns the detailed source/store hostPath and security boundary.
# Keep it composed here rather than duplicating its durable-log assertions.
bash "$SCRIPT_DIR/log-collector-contract.sh"

# Disabled defaults must not install either operator-opt-in component.
helm template incident-defaults "$CHART_DIR" --is-upgrade >"$render_dir/default.yaml"
if grep -Eq 'name: .*-(prometheus|alertmanager|monitoring|log-collector)' "$render_dir/default.yaml"; then
  echo 'FAIL: disabled observability defaults rendered an opt-in component' >&2
  exit 1
fi

# Schema accept fixture carries all values needed for the enabled render.
helm template incident-enabled "$CHART_DIR" \
  --is-upgrade \
  -f "$FIXTURES/incident-observability-schema-accept.yaml" \
  --show-only templates/deployment-monitoring.yaml \
  --show-only templates/configmap-monitoring.yaml \
  --show-only templates/daemonset-log-collector.yaml \
  >"$render_dir/enabled.yaml"

# Every reject fixture is an independently invalid values document. Helm must
# reject it before templates can accidentally normalize an unsafe value.
for fixture in "$FIXTURES"/incident-observability-schema-reject-*.yaml; do
  if helm template incident-rejected "$CHART_DIR" -f "$fixture" >/dev/null 2>&1; then
    echo "FAIL: schema accepted ${fixture##*/}" >&2
    exit 1
  fi
done

python3 - "$render_dir/enabled.yaml" <<'PY'
import sys
from pathlib import Path

text = Path(sys.argv[1]).read_text()

def require(value):
    assert value in text, value

# Single replicas, immutable image references, exact local retention, bounded
# values, and the externally supplied Secret reference are rendered literally.
for value in (
    'name: incident-enabled-djinn-prometheus',
    'name: incident-enabled-djinn-alertmanager',
    'replicas: 1', 'image: "prom/prometheus:v2.54.1"',
    'image: "prom/alertmanager:v0.27.0"',
    '--storage.tsdb.retention.time=7d', 'secretName: "incident-webhook"',
    'key: "url"', 'path: url',
    'djinn_server_memory_limit_bytes > 0.85', 'sizeLimit: 64Mi',
    'value: /store/logs', 'mountPath: /source/pods', 'mountPath: /store',
):
    require(value)
assert ':latest' not in text

# Both scrapes retain the 30-second global/evaluation cadence and the expected
# server and rotator jobs; an accidental one-job render is not sufficient.
for value in (
    'scrape_interval: 30s, evaluation_interval: 30s',
    'job_name: djinn-server', 'job_name: djinn-log-rotator',
    'metrics_path: /metrics', ':9091',
):
    require(value)

# Monitoring state is deliberately bounded to the chart's seven-day retention;
# it is local emptyDir state, not a claim that Helm provisions durable disks.
assert 'emptyDir: {}' in text
print('PASS: helm_contract::incident_observability')
PY
