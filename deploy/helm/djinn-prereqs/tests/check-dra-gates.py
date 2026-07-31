#!/usr/bin/env python3
"""Assert the two facts about djinn-prereqs that a fake kubectl cannot see.

Both were found by the FIRST real-cluster install of this chart (task srdw,
2026-07-30). Everything before that had been proven against `helm template`
output and fake-kubectl fixtures only, which is precisely why both survived to
production readiness.

CHECK 1 — Dynamic Resource Allocation is switched OFF.
------------------------------------------------------
Kueue 0.19.0 ships `KueueDRAIntegration` ENABLED. With it on, the manager
builds a ResourceSlice indexer against `resource.k8s.io/v1` unconditionally.
That API group is GA only in Kubernetes 1.34, so on anything older the process
exits at startup:

    "msg":"Unable to setup indexes","error":"could not setup ResourceSlice
     indexer: no matches for kind \\"ResourceSlice\\" in version
     \\"resource.k8s.io/v1\\""

The Deployment then CrashLoopBackOffs, `helm --wait` never returns, and the
release is left with all 11 CRDs Established and BOTH webhook configurations
registered behind a dead controller. Reproduced on a disposable kind cluster at
1.31.0 — the repo's own pin in scripts/kind/setup-kind.sh.

All THREE gates must be off. `KueueDRAIntegrationExtendedResource` and
`KueueDRAIntegrationPartitionableDevices` default to enabled and each declares
a hard dependency on the parent gate, so disabling only the parent makes the
manager reject its own flags ("conflicting feature gates detected"). A checker
that only looked for `KueueDRAIntegration=false` would pass a render that
cannot start.

This check fails if any of the three is missing from the rendered
`--feature-gates` flag or is set to anything but `false`, so an upstream
default cannot silently restore the 1.34 floor on a chart bump.

CHECK 2 — the disabled-framework webhooks are REGISTERED, with Ignore.
----------------------------------------------------------------------
values.yaml used to claim that dropping pod/deployment/statefulset from
`integrations.frameworks` "deletes mpod/vpod, mdeployment/vdeployment and
mstatefulset/vstatefulset from the rendered MutatingWebhookConfiguration and
ValidatingWebhookConfiguration outright". That is false, and it was the text an
operator read as the scoping policy. Disproven by `helm template` AND by the
live cluster: all six ARE registered. The upstream template branch
(`templates/webhook/manifests.yaml`) is a failurePolicy switch, not a
registration guard:

    {{- if has "pod" ... }} failurePolicy: Fail {{- else }}
    failurePolicy: Ignore {{- end }}

So this check asserts what is actually true and load-bearing: those six
webhooks are PRESENT and carry `failurePolicy: Ignore`. `Ignore` is the real
availability guarantee — a Kueue webhook whose handler the manager never
enabled is skipped rather than fatal. Asserting presence (not absence) is what
stops the prose from drifting back to the comfortable-but-wrong version.

`mjob`/`vjob` are deliberately excluded: batch/job is the framework Djinn will
use at cutover, so `Fail` there is correct and the positive namespace fence is
what keeps it safe. That fence is checked by
deploy/kueue/tests/check-webhook-selectors.py, not here.

Usage: check-dra-gates.py <rendered-manifest.yaml>
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

try:
    import yaml
except ImportError:  # pragma: no cover
    print("FAIL: PyYAML is required by this checker.", file=sys.stderr)
    raise SystemExit(2)

# Every gate that must be OFF. The two children are not decoration: each
# declares `requires KueueDRAIntegration to be enabled`, so leaving either on
# while the parent is off is a startup-fatal flag conflict, not a no-op.
REQUIRED_DISABLED_GATES = (
    "KueueDRAIntegration",
    "KueueDRAIntegrationExtendedResource",
    "KueueDRAIntegrationPartitionableDevices",
)

GATE_FLAG = "--feature-gates="

# Webhook name prefixes that must be registered with failurePolicy: Ignore,
# split by the configuration kind they belong to.
IGNORE_MUTATING = ("mpod", "mdeployment", "mstatefulset")
IGNORE_VALIDATING = ("vpod", "vdeployment", "vstatefulset")


def load_docs(path: Path) -> list[dict]:
    with path.open(encoding="utf-8") as handle:
        return [doc for doc in yaml.safe_load_all(handle) if isinstance(doc, dict)]


def manager_args(docs: list[dict]) -> list[str]:
    """Return the args of the Kueue controller-manager container.

    Selected by the `/manager` command rather than by a name match, so a
    release-name or label change cannot make this checker quietly find nothing
    and report success.
    """
    for doc in docs:
        if doc.get("kind") != "Deployment":
            continue
        spec = doc.get("spec", {}).get("template", {}).get("spec", {})
        for container in spec.get("containers", []) or []:
            if "/manager" in (container.get("command") or []):
                return list(container.get("args") or [])
    return []


def check_gates(docs: list[dict]) -> list[str]:
    failures: list[str] = []
    args = manager_args(docs)
    if not args:
        return ["no container running /manager was rendered; the checker had nothing to assert on"]

    flags = [a for a in args if a.startswith(GATE_FLAG)]
    if not flags:
        return [
            "the controller-manager renders no --feature-gates flag. Kueue 0.19 defaults "
            "KueueDRAIntegration to ENABLED, so this install requires Kubernetes >= 1.34 "
            "and CrashLoopBackOffs below it. Restore kueue.controllerManager.featureGates "
            "in values.yaml."
        ]
    if len(flags) > 1:
        failures.append(f"expected exactly one --feature-gates flag, got {len(flags)}: {flags}")

    parsed: dict[str, str] = {}
    for flag in flags:
        for pair in flag[len(GATE_FLAG):].split(","):
            pair = pair.strip()
            if not pair:
                continue
            name, _, value = pair.partition("=")
            parsed[name.strip()] = value.strip()

    for gate in REQUIRED_DISABLED_GATES:
        actual = parsed.get(gate)
        if actual is None:
            failures.append(
                f"feature gate {gate} is not disabled in the rendered --feature-gates flag "
                f"(saw: {sorted(parsed)}). Kueue 0.19 defaults it ON, which forces a "
                f"Kubernetes 1.34 floor."
            )
        elif actual != "false":
            failures.append(
                f"feature gate {gate} is set to {actual!r}; it must be 'false'. Enabling DRA "
                f"integration makes the manager index resource.k8s.io/v1 ResourceSlices, which "
                f"exist only on Kubernetes >= 1.34."
            )
    return failures


def check_ignore_webhooks(docs: list[dict]) -> list[str]:
    failures: list[str] = []
    for kind, expected in (
        ("MutatingWebhookConfiguration", IGNORE_MUTATING),
        ("ValidatingWebhookConfiguration", IGNORE_VALIDATING),
    ):
        found: dict[str, str | None] = {}
        for doc in docs:
            if doc.get("kind") != kind:
                continue
            for hook in doc.get("webhooks") or []:
                name = hook.get("name", "")
                found[name.split(".")[0]] = hook.get("failurePolicy")

        for short in expected:
            if short not in found:
                failures.append(
                    f"{short} is ABSENT from the rendered {kind}. It must be PRESENT: the "
                    f"upstream chart renders it unconditionally and uses integrations.frameworks "
                    f"only to switch failurePolicy. If this ever legitimately changes, fix the "
                    f"scoping note in values.yaml in the same commit."
                )
                continue
            policy = found[short]
            if policy != "Ignore":
                failures.append(
                    f"{short} in the rendered {kind} has failurePolicy={policy!r}, expected "
                    f"'Ignore'. 'Fail' means its framework was re-added to "
                    f"integrations.frameworks, pointing a fail-closed admission webhook at core "
                    f"Kubernetes CREATE."
                )
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path, help="rendered djinn-prereqs manifest")
    args = parser.parse_args()

    if not args.manifest.is_file():
        print(f"FAIL: rendered manifest not found: {args.manifest}", file=sys.stderr)
        return 2

    docs = load_docs(args.manifest)
    if not docs:
        print(f"FAIL: {args.manifest} contains no Kubernetes objects", file=sys.stderr)
        return 2

    failures = check_gates(docs) + check_ignore_webhooks(docs)
    for failure in failures:
        print(f"FAIL: {failure}", file=sys.stderr)
    if failures:
        return 1

    print(
        "OK: DRA feature gates disabled "
        f"({', '.join(REQUIRED_DISABLED_GATES)}) and "
        f"{', '.join(IGNORE_MUTATING + IGNORE_VALIDATING)} registered with failurePolicy Ignore."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
