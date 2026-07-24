#!/usr/bin/env bash
# Exact Helm render contract for the shared-volume ownership guarantee (pwrr).
#
# The mirrors/cache/projects volumes are written by the server (uid 10001) AND
# by task-run/warm Job Pods (uid/gid 1000 + child uid 1001, group 1000). The
# chart must therefore render, on a fresh install:
#   * supplementary membership in the artifact GID on the server pod — and NOT
#     `fsGroup`, whose kubelet ownership pass is an unbounded recursive chown;
#   * a root initContainer that normalizes the three volume ROOTS to
#     `ownerUid:artifactGid` with setgid + group-write, non-recursively.
#
# Usage: bash deploy/helm/djinn/tests/volume-ownership-render.sh
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
require_tool() { command -v "$1" >/dev/null 2>&1 || { echo "FAIL: required test tool '$1' is not installed" >&2; exit 1; }; }
require_tool helm
require_tool python3
TMPDIR_RENDER=$(mktemp -d)
trap 'rm -rf "$TMPDIR_RENDER"' EXIT

render() {
    helm template volume-ownership-test "$CHART_DIR" \
        --set-string migration.designatedOperatorSecret=volume-ownership-test-operator \
        "$@" > "$TMPDIR_RENDER/manifest.yaml"
}

assert_contract() {
    python3 - "$TMPDIR_RENDER/manifest.yaml" "$@" <<'PY'
import sys
import yaml

manifest, expect_init = sys.argv[1], sys.argv[2] == "with-init"
docs = [d for d in yaml.safe_load_all(open(manifest, encoding="utf-8")) if d]
deps = [
    d
    for d in docs
    if d.get("kind") == "Deployment" and "server" in d["metadata"]["name"]
]
assert len(deps) == 1, f"expected exactly one server Deployment, got {len(deps)}"
spec = deps[0]["spec"]["template"]["spec"]

sc = spec.get("securityContext") or {}
assert sc.get("supplementalGroups") == [1000], f"bad supplementalGroups: {sc}"
assert "fsGroup" not in sc, "fsGroup must not be set: it triggers a recursive chown"

inits = {c["name"]: c for c in (spec.get("initContainers") or [])}
if not expect_init:
    assert "fix-volume-perms" not in inits, "normalizeRoots=false must drop the initContainer"
    assert "migrate" in inits, "the remaining initContainers must still render"
    print("OK: normalizeRoots=false renders without fix-volume-perms")
    sys.exit(0)

fix = inits.get("fix-volume-perms")
assert fix is not None, f"fix-volume-perms missing, got {list(inits)}"
assert fix["securityContext"]["runAsUser"] == 0, "root is required to chown the roots"
script = fix["command"][-1]
assert "chown 10001:1000 " in script, f"root ownership not declared: {script}"
assert "chmod 2775 " in script, f"setgid + group-write not declared: {script}"
assert " -R" not in script and "-R " not in script, "the normalization must never recurse"
for root in ("mirrors", "cache", "projects"):
    assert f"/var/lib/djinn/{root}" in script, f"{root} root not normalized"
    assert any(
        m["mountPath"] == f"/var/lib/djinn/{root}" for m in fix["volumeMounts"]
    ), f"{root} not mounted into the initContainer"
print("OK: fresh install declares the artifact-GID / g+w / setgid contract")
PY
}

render
assert_contract with-init

render --set storage.ownership.normalizeRoots=false
assert_contract without-init

echo "PASS: volume ownership render contract"
