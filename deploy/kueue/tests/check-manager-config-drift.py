#!/usr/bin/env python3
"""Prove djinn-prereqs' Kueue manager config is upstream's, plus three edits.

The upstream Kueue chart exposes `managedJobsNamespaceSelector` and
`integrations.frameworks` only inside one opaque string value
(`managerConfig.controllerManagerConfigYaml`). Overriding either forces us to
restate the ENTIRE default. That is a silent-drift trap: on a version bump the
subchart's default can change underneath a copy that keeps rendering happily.

This check reads the subchart's own default out of the pinned
`charts/kueue-<version>.tgz` and the override out of the wrapper's
`values.yaml`, parses both, and requires the difference to be EXACTLY:

  1. `managedJobsNamespaceSelector` — absent upstream, set by us to the
     positive `djinn.io/kueue-managed: "true"` matchLabels fence.
  2. `integrations.frameworks` — ours is upstream's list minus exactly
     {pod, deployment, statefulset}, order preserved.
  3. `waitForPodsReady` — absent upstream, set by us to the exact bounded
     readiness-recovery policy.

Any other difference fails. On a bump, the diff it prints is the review.

Usage: check-manager-config-drift.py <chart-dir>
"""

from __future__ import annotations

import argparse
import sys
import tarfile
from pathlib import Path
from typing import Any

try:
    import yaml
except ImportError:  # pragma: no cover
    print("FAIL: PyYAML is required by this checker.", file=sys.stderr)
    raise SystemExit(2)

CONFIG_PATH = ("managerConfig", "controllerManagerConfigYaml")
SELECTOR_KEY = "managedJobsNamespaceSelector"
EXPECTED_SELECTOR = {"matchLabels": {"djinn.io/kueue-managed": "true"}}
REMOVED_FRAMEWORKS = ["pod", "deployment", "statefulset"]
WAIT_FOR_PODS_READY_KEY = "waitForPodsReady"
EXPECTED_WAIT_FOR_PODS_READY = {
    "timeout": "30m",
    "recoveryTimeout": "3m",
    "blockAdmission": False,
    "requeuingStrategy": {
        "timestamp": "Eviction",
        "backoffLimitCount": None,
        "backoffBaseSeconds": 60,
        "backoffMaxSeconds": 3600,
    },
}


def dig(tree: Any, path: tuple[str, ...]) -> Any:
    for key in path:
        if not isinstance(tree, dict) or key not in tree:
            raise KeyError(".".join(path))
        tree = tree[key]
    return tree


def subchart_values(chart_dir: Path) -> tuple[str, dict[str, Any]]:
    tarballs = sorted(chart_dir.glob("charts/kueue-*.tgz"))
    if len(tarballs) != 1:
        raise SystemExit(
            f"FAIL: expected exactly one vendored kueue dependency tarball in "
            f"{chart_dir}/charts, found {[t.name for t in tarballs]}. Run "
            f"`helm dependency update {chart_dir}` and commit the result."
        )
    tarball = tarballs[0]
    with tarfile.open(tarball) as archive:
        member = archive.extractfile("kueue/values.yaml")
        if member is None:
            raise SystemExit(f"FAIL: {tarball.name} has no kueue/values.yaml")
        return tarball.name, yaml.safe_load(member.read().decode("utf-8"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("chart_dir", type=Path)
    args = parser.parse_args()
    chart_dir: Path = args.chart_dir

    tarball_name, upstream_values = subchart_values(chart_dir)
    wrapper_values = yaml.safe_load((chart_dir / "values.yaml").read_text("utf-8"))

    try:
        upstream_raw = dig(upstream_values, CONFIG_PATH)
        ours_raw = dig(wrapper_values["kueue"], CONFIG_PATH)
    except KeyError as missing:
        print(f"FAIL: {missing} is missing from one of the value trees", file=sys.stderr)
        return 1

    upstream = yaml.safe_load(upstream_raw)
    ours = yaml.safe_load(ours_raw)

    failures: list[str] = []

    if SELECTOR_KEY in upstream:
        failures.append(
            f"upstream {tarball_name} now sets {SELECTOR_KEY} itself "
            f"({upstream[SELECTOR_KEY]!r}); re-derive the override deliberately"
        )
    if ours.get(SELECTOR_KEY) != EXPECTED_SELECTOR:
        failures.append(
            f"{SELECTOR_KEY} must be {EXPECTED_SELECTOR!r}, got {ours.get(SELECTOR_KEY)!r}"
        )

    if WAIT_FOR_PODS_READY_KEY in upstream:
        failures.append(
            f"upstream {tarball_name} now sets {WAIT_FOR_PODS_READY_KEY} itself "
            f"({upstream[WAIT_FOR_PODS_READY_KEY]!r}); re-derive the override deliberately"
        )
    if ours.get(WAIT_FOR_PODS_READY_KEY) != EXPECTED_WAIT_FOR_PODS_READY:
        failures.append(
            f"{WAIT_FOR_PODS_READY_KEY} must be "
            f"{EXPECTED_WAIT_FOR_PODS_READY!r}, got "
            f"{ours.get(WAIT_FOR_PODS_READY_KEY)!r}"
        )

    upstream_frameworks = (upstream.get("integrations") or {}).get("frameworks") or []
    our_frameworks = (ours.get("integrations") or {}).get("frameworks") or []
    expected_frameworks = [f for f in upstream_frameworks if f not in REMOVED_FRAMEWORKS]
    still_removed = [f for f in REMOVED_FRAMEWORKS if f in upstream_frameworks]
    if not still_removed:
        failures.append(
            f"upstream {tarball_name} no longer enables any of {REMOVED_FRAMEWORKS} "
            "by default; this override may now be a no-op that hides a real change"
        )
    if our_frameworks != expected_frameworks:
        failures.append(
            "integrations.frameworks must be upstream's list minus "
            f"{REMOVED_FRAMEWORKS}.\n         expected: {expected_frameworks}\n"
            f"         got:      {our_frameworks}"
        )

    # Everything else must be byte-for-byte equivalent after parsing.
    def stripped(tree: dict[str, Any]) -> dict[str, Any]:
        copy = dict(tree)
        copy.pop(SELECTOR_KEY, None)
        copy.pop(WAIT_FOR_PODS_READY_KEY, None)
        integrations = dict(copy.get("integrations") or {})
        integrations.pop("frameworks", None)
        if integrations:
            copy["integrations"] = integrations
        else:
            copy.pop("integrations", None)
        return copy

    upstream_rest, our_rest = stripped(upstream), stripped(ours)
    if upstream_rest != our_rest:
        only_upstream = {k: v for k, v in upstream_rest.items() if our_rest.get(k) != v}
        only_ours = {k: v for k, v in our_rest.items() if upstream_rest.get(k) != v}
        failures.append(
            "controllerManagerConfigYaml drifted from upstream outside the three "
            f"sanctioned edits.\n         upstream-only/changed: {only_upstream}\n"
            f"         ours-only/changed:     {only_ours}"
        )

    if failures:
        for failure in failures:
            print(f"FAIL: {failure}", file=sys.stderr)
        return 1

    print(
        f"PASS: {chart_dir.name} manager config equals {tarball_name}'s default "
        f"plus exactly three edits ({SELECTOR_KEY} fence, frameworks minus "
        f"{REMOVED_FRAMEWORKS}, {WAIT_FOR_PODS_READY_KEY} recovery policy)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
