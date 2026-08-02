#!/usr/bin/env bash
# Exercise the public finite Kueue capacity-vector values contract with Helm's
# actual draft-07 schema validator. These are lint cases, not template-only
# assertions: an operator receives the same errors before an install.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

lint() {
  helm lint "$CHART_DIR" --strict "$@"
}

expect_rejected() {
  local name=$1
  shift
  if lint "$@" >"$WORK/$name.out" 2>&1; then
    echo "FAIL: helm lint accepted invalid capacity-vector case '$name'" >&2
    exit 1
  fi
  printf 'PASS: rejected %s\n' "$name"
}

# The production defaults and local overlay remain schema-valid. Exercise the
# conditional's positive branch as well: a dedicated named NodePool supplies a
# complete static fallback vector from the defaults.
lint
lint --values "$CHART_DIR/values.local.yaml"
lint \
  --set kueue.capacity.source=nodepool-limits \
  --set kueue.capacity.nodePool.dedicated=true \
  --set kueue.capacity.nodePool.name=djinn-build

expect_rejected malformed-build-cpu --set-string kueue.buildCpu=not-a-quantity
expect_rejected malformed-build-memory --set-string kueue.buildMemory=not-a-quantity
expect_rejected malformed-headroom-cpu --set-string kueue.capacity.headroom.cpu=not-a-quantity
expect_rejected malformed-static-memory --set-string kueue.capacity.staticFallback.memory=not-a-quantity
expect_rejected fractional-build-pods --set kueue.buildPods=1.5
expect_rejected string-build-pods --set-string kueue.buildPods=three
expect_rejected unknown-source --set-string kueue.capacity.source=observed
expect_rejected legacy-binding-resource --set-string kueue.bindingResource=cpu
expect_rejected nodepool-not-dedicated \
  --set kueue.capacity.source=nodepool-limits \
  --set kueue.capacity.nodePool.dedicated=false \
  --set kueue.capacity.nodePool.name=djinn-build
expect_rejected nodepool-empty-name \
  --set kueue.capacity.source=nodepool-limits \
  --set kueue.capacity.nodePool.dedicated=true \
  --set-string kueue.capacity.nodePool.name=

# Helm --set cannot remove a default, so make copies of the real chart values
# for required-field cases. The copied chart retains the real schema and all
# template behavior; only the named contract field is absent.
make_chart_without() {
  local name=$1 path=$2 source=${3:-node-sum}
  local chart="$WORK/$name"
  cp -R "$CHART_DIR" "$chart"
  python3 - "$chart/values.yaml" "$path" "$source" <<'PY'
import sys
import yaml

values_path, dotted_path, source = sys.argv[1:]
with open(values_path, encoding="utf-8") as stream:
    values = yaml.safe_load(stream)
values["kueue"]["capacity"]["source"] = source
cursor = values
parts = dotted_path.split(".")
for part in parts[:-1]:
    cursor = cursor[part]
cursor.pop(parts[-1])
with open(values_path, "w", encoding="utf-8") as stream:
    yaml.safe_dump(values, stream, sort_keys=False)
PY
  printf '%s\n' "$chart"
}

for field in buildCpu buildMemory; do
  chart=$(make_chart_without "missing-$field" "kueue.$field")
  if helm lint "$chart" --strict >"$WORK/missing-$field.out" 2>&1; then
    echo "FAIL: helm lint accepted missing kueue.$field" >&2
    exit 1
  fi
done

for field in cpu memory pods; do
  chart=$(make_chart_without "missing-static-$field" "kueue.capacity.staticFallback.$field" static)
  if helm lint "$chart" --strict >"$WORK/missing-static-$field.out" 2>&1; then
    echo "FAIL: helm lint accepted incomplete static fallback missing $field" >&2
    exit 1
  fi
  chart=$(make_chart_without "missing-nodepool-$field" "kueue.capacity.staticFallback.$field" nodepool-limits)
  if helm lint "$chart" --strict >"$WORK/missing-nodepool-$field.out" 2>&1; then
    echo "FAIL: helm lint accepted nodepool-limits fallback missing $field" >&2
    exit 1
  fi
done

echo "PASS: finite Kueue capacity-vector schema accepts valid values and rejects malformed vectors"
