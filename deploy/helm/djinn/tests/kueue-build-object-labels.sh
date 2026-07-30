#!/usr/bin/env bash
# Structural render contract: this inert release must not opt control-plane Pods
# into the Kueue build-object admission selector.
# Usage: bash deploy/helm/djinn/tests/kueue-build-object-labels.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

require_tool() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "FAIL: required test tool '$1' is not installed" >&2
        exit 1
    }
}

require_tool helm
require_tool python3
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

helm template kueue-build-object-test "$CHART_DIR" --is-upgrade >"$WORK/rendered.yaml"

python3 - "$WORK/rendered.yaml" <<'PY'
import sys
import yaml

RESERVED_LABEL = "djinn.io/kueue-build-object"
WORKLOAD_KINDS = {"Deployment", "StatefulSet", "DaemonSet", "Job"}


def labelled_pod_templates(docs):
    """Return workload names whose Pod template selects Kueue build admission."""
    violations = []
    for doc in docs:
        if doc.get("kind") not in WORKLOAD_KINDS:
            continue
        labels = (
            doc.get("spec", {})
            .get("template", {})
            .get("metadata", {})
            .get("labels", {})
        )
        if labels.get(RESERVED_LABEL) == "true":
            violations.append(doc.get("metadata", {}).get("name", "<unnamed>"))
    return violations


docs = [doc for doc in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")) if doc]
workloads = [doc for doc in docs if doc.get("kind") in WORKLOAD_KINDS]
assert workloads, "expected rendered control-plane Pod-template workloads"
assert not labelled_pod_templates(docs), (
    "control-plane Pod templates must not carry "
    f"{RESERVED_LABEL}=true: {labelled_pod_templates(docs)}"
)

# Non-vacuity: prove the same structural scanner rejects an explicitly labelled
# manifest fixture rather than passing merely because current templates lack it.
negative_fixture = {
    "apiVersion": "apps/v1",
    "kind": "Deployment",
    "metadata": {"name": "negative-labelled-fixture"},
    "spec": {
        "template": {
            "metadata": {"labels": {RESERVED_LABEL: "true"}},
            "spec": {"containers": []},
        }
    },
}
assert labelled_pod_templates([negative_fixture]) == ["negative-labelled-fixture"], (
    "scanner must reject the explicit reserved-label fixture"
)
PY

echo "=== Kueue build-object label render contract passed ==="
