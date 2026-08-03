#!/usr/bin/env bash
# Validate the auditable vector-capacity release contract and prove its partial
# rollback guards fail closed. This script is glob-discovered by
# scripts/test-helm-chart-contracts.sh.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../../.." && pwd)"
MANIFEST="$REPO_ROOT/deploy/helm/djinn/capacity-release-contract.yaml"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# CI checks pull-request merge refs with a depth-one checkout. The recorded
# #2901 commit is outside that shallow boundary, so make the ancestry evidence
# available before the required merge-base check. Mutation cases deliberately
# use invalid object IDs and remain unfetched/rejected by the validator below.
if [ "$(git -C "$REPO_ROOT" rev-parse --is-shallow-repository)" = "true" ]; then
  if ! git -C "$REPO_ROOT" fetch --no-tags --unshallow origin; then
    echo "FAIL: ANCESTRY_HISTORY_UNAVAILABLE: cannot deepen shallow checkout for #2901" >&2
    exit 1
  fi
fi

# PyYAML is the repository's existing YAML mechanism used by the neighboring
# Helm contract scripts and Kueue drift checks. Keeping validation in Python
# makes nested mutations structural instead of line-oriented grep substitutions.
cat >"$WORK/validate.py" <<'PY'
from __future__ import annotations

import copy
import re
import subprocess
import sys
from pathlib import Path

import yaml

SHA = "deade196e6f6bc14dea3f0c2ebfcb45a6481f3f1"
EXPECTED_PATHS = {
    "controller": ["server/crates/djinn-k8s/src/capacity_controller.rs"],
    "chartValues": ["deploy/helm/djinn/values.yaml"],
    "schema": ["deploy/helm/djinn/values.schema.json"],
    "serverTemplate": ["deploy/helm/djinn/templates/deployment-server.yaml"],
    "topology": ["deploy/helm/djinn/templates/kueue-topology.yaml"],
    "rbac": ["deploy/helm/djinn/templates/clusterrole-capacity.yaml"],
    "mixedVersionTests": [
        "deploy/helm/djinn/tests/capacity-mixed-version-contract.sh",
        "deploy/helm/djinn/tests/fixtures/capacity-old-chart-pr2901.yaml",
    ],
    "releaseTests": ["deploy/helm/djinn/tests/capacity-release-contract.sh"],
    "prereqsConfiguration": ["deploy/helm/djinn-prereqs/values.yaml"],
    "driftChecker": ["deploy/kueue/tests/check-manager-config-drift.py"],
}


class ContractError(Exception):
    def __init__(self, code: str, detail: str):
        self.code = code
        super().__init__(f"{code}: {detail}")


def require(condition: bool, code: str, detail: str) -> None:
    if not condition:
        raise ContractError(code, detail)


def validate(data: object, repo_root: Path, check_ancestor: bool) -> None:
    require(isinstance(data, dict), "MANIFEST_NOT_MAPPING", "manifest must be a YAML mapping")
    require(
        set(data) == {"pr2901Merge", "oru9OwnedItems", "capacityContract", "sentinelRemoval", "requiredPaths"},
        "MANIFEST_KEYS", "manifest must contain exactly the release-contract keys",
    )
    sha = data["pr2901Merge"]
    require(isinstance(sha, str) and re.fullmatch(r"[0-9a-f]{40}", sha) is not None,
            "MALFORMED_SHA", "pr2901Merge must be a lowercase 40-hex SHA")
    if check_ancestor:
        result = subprocess.run(
            ["git", "merge-base", "--is-ancestor", sha, "HEAD"], cwd=repo_root, check=False
        )
        require(result.returncode == 0, "NON_ANCESTOR_SHA", "recorded #2901 merge is not an ancestor of HEAD")
    require(sha == SHA, "WRONG_PR2901_SHA", "pr2901Merge must record the #2901 merge")

    require(data["oru9OwnedItems"] == [1, 2], "WRONG_OWNED_ITEMS", "oru9OwnedItems must be exactly [1, 2]")

    capacity = data["capacityContract"]
    require(isinstance(capacity, dict), "CAPACITY_CONTRACT_SHAPE", "capacityContract must be a mapping")
    require(set(capacity) == {"active", "staticFallback", "ownership", "legacyCompatibility"},
            "CAPACITY_CONTRACT_KEYS", "capacityContract must declare activation, vector, ownership, and compatibility")
    static = capacity["staticFallback"]
    require(isinstance(static, dict), "STATIC_FALLBACK_SHAPE", "staticFallback must be a mapping")
    for dimension in ("cpu", "memory", "pods"):
        require(static.get(dimension) == "finite", f"VECTOR_STATIC_{dimension.upper()}_REQUIRED",
                f"vector-v1 requires finite static fallback {dimension}")
    require(set(static) == {"cpu", "memory", "pods"}, "STATIC_FALLBACK_KEYS",
            "staticFallback must cover exactly cpu, memory, and pods")

    ownership = capacity["ownership"]
    require(isinstance(ownership, dict), "OWNERSHIP_SHAPE", "ownership must be a mapping")
    require(ownership.get("selector") == "required", "OWNERSHIP_SELECTOR_REQUIRED",
            "vector-v1 requires a ResourceFlavor ownership selector")
    require(ownership.get("dedicationOrIdentity") == "required", "OWNERSHIP_IDENTITY_REQUIRED",
            "vector-v1 requires dedicated-pool or explicit scheduling identity")
    require(set(ownership) == {"selector", "dedicationOrIdentity"}, "OWNERSHIP_KEYS",
            "ownership must cover selector and dedication/identity")
    require(capacity["legacyCompatibility"] == "one-release", "LEGACY_COMPATIBILITY_REQUIRED",
            "#2901 legacy compatibility must remain for one release")

    sentinel = data["sentinelRemoval"]
    require(isinstance(sentinel, dict) and sentinel == {"policy": "vector-v1-complete-only"},
            "SENTINEL_REMOVAL_UNSAFE", "sentinel removal requires complete vector-v1 configuration")
    require(capacity["active"] == "vector-v1" and all(static.get(d) == "finite" for d in ("cpu", "memory", "pods"))
            and ownership.get("selector") == "required" and ownership.get("dedicationOrIdentity") == "required"
            and capacity["legacyCompatibility"] == "one-release", "SENTINEL_REMOVAL_UNSAFE",
            "sentinel removal cannot precede complete vector-capable configuration")
    require(capacity["active"] == "vector-v1", "ACTIVE_VECTOR_V1_REQUIRED",
            "the active declaration must be vector-v1")

    paths = data["requiredPaths"]
    require(paths == EXPECTED_PATHS, "REQUIRED_PATH_COVERAGE", "requiredPaths must exactly cover all coordinated path classes")
    for path_class, paths_in_class in EXPECTED_PATHS.items():
        for relative_path in paths_in_class:
            require((repo_root / relative_path).is_file(), "REQUIRED_PATH_MISSING",
                    f"{path_class} path does not exist: {relative_path}")


def mutation(data: dict, name: str) -> dict:
    result = copy.deepcopy(data)
    if name == "malformed-sha":
        result["pr2901Merge"] = "not-a-sha"
    elif name == "non-ancestor-sha":
        result["pr2901Merge"] = "0" * 40
    elif name == "wrong-owned-items":
        result["oru9OwnedItems"] = [1]
    elif name == "missing-required-path-class":
        del result["requiredPaths"]["rbac"]
    elif name == "sentinel-without-vector":
        result["capacityContract"]["active"] = "legacy"
    elif name.startswith("missing-static-"):
        del result["capacityContract"]["staticFallback"][name.removeprefix("missing-static-")]
    elif name == "missing-ownership-selector":
        del result["capacityContract"]["ownership"]["selector"]
    elif name == "missing-dedication-identity":
        del result["capacityContract"]["ownership"]["dedicationOrIdentity"]
    elif name == "dropped-compatibility":
        result["capacityContract"]["legacyCompatibility"] = "none"
    else:
        raise AssertionError(name)
    return result


def main() -> int:
    manifest, repo_root, work_dir, *case = map(Path, sys.argv[1:])
    try:
        data = yaml.safe_load(manifest.read_text(encoding="utf-8"))
        if case:
            name = str(case[0])
            mutated_contract = work_dir / f"{name}.yaml"
            mutated_contract.write_text(yaml.safe_dump(mutation(data, name), sort_keys=False), encoding="utf-8")
            validate(yaml.safe_load(mutated_contract.read_text(encoding="utf-8")), repo_root,
                     name == "non-ancestor-sha")
        else:
            validate(data, repo_root, True)
    except ContractError as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    print("PASS: capacity release contract is complete")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
PY

python3 "$WORK/validate.py" "$MANIFEST" "$REPO_ROOT" "$WORK"

expect_rejected() {
  local name=$1 diagnostic=$2
  if python3 "$WORK/validate.py" "$MANIFEST" "$REPO_ROOT" "$WORK" "$name" >"$WORK/$name.out" 2>&1; then
    echo "FAIL: mutation was accepted: $name" >&2
    exit 1
  fi
  if ! grep -Fq "$diagnostic" "$WORK/$name.out"; then
    echo "FAIL: mutation $name did not report $diagnostic" >&2
    cat "$WORK/$name.out" >&2
    exit 1
  fi
  printf 'PASS: rejected %s (%s)\n' "$name" "$diagnostic"
}

expect_rejected malformed-sha MALFORMED_SHA
expect_rejected non-ancestor-sha NON_ANCESTOR_SHA
expect_rejected wrong-owned-items WRONG_OWNED_ITEMS
expect_rejected missing-required-path-class REQUIRED_PATH_COVERAGE
expect_rejected sentinel-without-vector SENTINEL_REMOVAL_UNSAFE
for dimension in cpu memory pods; do
  expect_rejected "missing-static-$dimension" "VECTOR_STATIC_${dimension^^}_REQUIRED"
done
expect_rejected missing-ownership-selector OWNERSHIP_SELECTOR_REQUIRED
expect_rejected missing-dedication-identity OWNERSHIP_IDENTITY_REQUIRED
expect_rejected dropped-compatibility LEGACY_COMPATIBILITY_REQUIRED

echo "PASS: release-contract partial activation and removal mutations fail closed"
