#!/usr/bin/env bash
# Focused values-schema contract for the task-run light CPU request.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SCHEMA="$CHART_DIR/values.schema.json"
command -v helm >/dev/null 2>&1 || { echo "FAIL: helm is required" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "FAIL: python3 is required" >&2; exit 1; }

TEMPORARY=$(mktemp -d)
trap 'rm -rf "$TEMPORARY"' EXIT

assert_schema_contract() {
  python3 - "$1" <<'PY'
import json
import sys

schema = json.load(open(sys.argv[1], encoding="utf-8"))
resources = schema["properties"]["resources"]
taskrun = resources["properties"]["taskrun"]
requests = taskrun["properties"]["requests"]
owned = {"cpu", "memory", "lightCpu"}

# Omitting additionalProperties means open under draft-07; explicitly true is
# open too. Only the request leaf is intentionally closed.
for name, value in (("chart root", schema), ("resources", resources), ("resources.taskrun", taskrun)):
    assert value.get("additionalProperties") is not False, f"{name} must stay open"
assert requests.get("additionalProperties") is False, "requests must be closed"
assert set(requests.get("required", ())) == owned, requests.get("required")
assert set(requests.get("properties", ())) == owned, requests.get("properties")
for name in ("cpu", "lightCpu"):
    leaf = requests["properties"][name]
    assert leaf.get("type") == "string", f"{name} must be a string"
    assert leaf.get("pattern"), f"{name} must validate Kubernetes quantities"
assert requests["properties"]["memory"].get("type") == "string"
PY
}

copy_chart() {
  local destination=$1
  cp -R "$CHART_DIR" "$destination"
}

# Pin the exact owned leaf and the deliberately-open compatibility boundaries.
assert_schema_contract "$SCHEMA"

# Stock quantities and an operator override must validate. The compatibility
# render explicitly carries existing task-run scheduling/limits and the server
# and warm resource trees, all of which remain open beneath resources.
helm template light-cpu-stock "$CHART_DIR" --is-upgrade --show-only templates/deployment-server.yaml >"$TEMPORARY/stock.yaml"
helm template light-cpu-override "$CHART_DIR" --is-upgrade \
  --set-string resources.taskrun.requests.lightCpu=450m \
  --show-only templates/deployment-server.yaml >"$TEMPORARY/override.yaml"
helm template light-cpu-compatibility "$CHART_DIR" --is-upgrade \
  --set-string resources.taskrun.limits.cpu=5 \
  --set-string resources.taskrun.limits.memory=24Gi \
  --set-string resources.taskrun.nodeSelector.workload-type=djinn \
  --set-string 'resources.taskrun.tolerations[0].key=workload-type' \
  --set-string 'resources.taskrun.tolerations[0].operator=Exists' \
  --set-string resources.server.limits.memory=3Gi \
  --set-string resources.warm.limits.cpu=3 \
  --show-only templates/deployment-server.yaml >"$TEMPORARY/compatibility.yaml"

if helm template light-cpu-invalid "$CHART_DIR" --is-upgrade \
  --set-string resources.taskrun.requests.lightCpu=bananas >"$TEMPORARY/invalid.out" 2>&1; then
  echo "FAIL: schema accepted lightCpu=bananas" >&2
  exit 1
fi
grep -q 'lightCpu' "$TEMPORARY/invalid.out" || {
  echo "FAIL: invalid lightCpu diagnostic did not name lightCpu" >&2
  cat "$TEMPORARY/invalid.out" >&2
  exit 1
}

# Mutate temporary chart copies so this contract fails if the owned property is
# removed or the closed leaf becomes permissive.
MISSING_PROPERTY="$TEMPORARY/chart-missing-light-cpu"
copy_chart "$MISSING_PROPERTY"
python3 - "$MISSING_PROPERTY/values.schema.json" <<'PY'
import json
import sys

path = sys.argv[1]
schema = json.load(open(path, encoding="utf-8"))
del schema["properties"]["resources"]["properties"]["taskrun"]["properties"]["requests"]["properties"]["lightCpu"]
with open(path, "w", encoding="utf-8") as output:
    json.dump(schema, output)
PY
if helm template missing-light-cpu "$MISSING_PROPERTY" --is-upgrade >"$TEMPORARY/missing-light-cpu.out" 2>&1; then
  echo "FAIL: stock values passed after removing the lightCpu schema property" >&2
  exit 1
fi
grep -Eq 'Additional property lightCpu is not allowed|lightCpu.*unexpected' "$TEMPORARY/missing-light-cpu.out" || {
  echo "FAIL: removed lightCpu property was not reported as unexpected" >&2
  cat "$TEMPORARY/missing-light-cpu.out" >&2
  exit 1
}

PERMISSIVE_REQUESTS="$TEMPORARY/chart-permissive-requests"
copy_chart "$PERMISSIVE_REQUESTS"
python3 - "$PERMISSIVE_REQUESTS/values.schema.json" <<'PY'
import json
import sys

path = sys.argv[1]
schema = json.load(open(path, encoding="utf-8"))
schema["properties"]["resources"]["properties"]["taskrun"]["properties"]["requests"]["additionalProperties"] = True
with open(path, "w", encoding="utf-8") as output:
    json.dump(schema, output)
PY
if assert_schema_contract "$PERMISSIVE_REQUESTS/values.schema.json"; then
  echo "FAIL: source-shape check did not detect permissive requests" >&2
  exit 1
fi
helm template permissive-requests "$PERMISSIVE_REQUESTS" --is-upgrade \
  --set-string resources.taskrun.requests.unowned=accepted >/dev/null || {
  echo "FAIL: mutated permissive requests schema did not accept an unowned value" >&2
  exit 1
}

# Closing any compatibility parent must be caught by the source-shape check.
for parent in root resources taskrun; do
  CLOSED_PARENT="$TEMPORARY/chart-closed-$parent"
  copy_chart "$CLOSED_PARENT"
  python3 - "$CLOSED_PARENT/values.schema.json" "$parent" <<'PY'
import json
import sys

path, parent = sys.argv[1:]
schema = json.load(open(path, encoding="utf-8"))
if parent == "root":
    target = schema
elif parent == "resources":
    target = schema["properties"]["resources"]
else:
    target = schema["properties"]["resources"]["properties"]["taskrun"]
target["additionalProperties"] = False
with open(path, "w", encoding="utf-8") as output:
    json.dump(schema, output)
PY
  if assert_schema_contract "$CLOSED_PARENT/values.schema.json"; then
    echo "FAIL: source-shape check did not detect closed $parent parent" >&2
    exit 1
  fi
done

echo "=== light CPU request schema Helm contract passed ==="
