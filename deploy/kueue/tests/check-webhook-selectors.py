#!/usr/bin/env python3
"""Check Djinn's Kueue admission scoping on a RENDERED djinn-prereqs manifest.

This checker takes the output of ``helm template`` over
``deploy/helm/djinn-prereqs`` — i.e. what a cluster actually receives. It used
to take ``deploy/kueue/vendor/kueue-v0.10.0.yaml``, a byte-vendored fork that
no longer exists. Pointing it at a static file that is not the deployment
artifact is the exact defect this rewrite removes: such a test stays green
while validating something the cluster never sees.

WHAT IS CHECKED
---------------
1. **Nothing selects Djinn's namespace.** With ``--namespace-labels`` the
   checker evaluates each relevant webhook's ``namespaceSelector`` as
   Kubernetes would, against the label set the ``djinn`` chart actually renders
   onto its Namespace, and fails if any of them matches. This is the assertion
   that bounds the availability blast radius, and it is checked directly rather
   than through a proxy.

   Stock upstream ships ``matchExpressions: kubernetes.io/metadata.name NotIn
   [kube-system, kueue-system]``, which DOES select ``djinn``. Combined with
   ``failurePolicy: Fail`` that makes an unavailable Kueue controller block Pod
   and Job creation in the Djinn namespace — a total-outage vector on a
   single-node cluster.

2. **Positive fence.** Every admission webhook whose rules cover CREATE on a
   *core Kubernetes* type Djinn creates or could create (``pods``, ``jobs``,
   ``deployments``, ``statefulsets``) must carry exactly

       namespaceSelector:
         matchLabels:
           djinn.io/kueue-managed: "true"

   A namespace must be explicitly labelled before Kueue can select anything in
   it. No asset in this repository applies that label.

3. **Disabled frameworks.** The ``pods``/``deployments``/``statefulsets``
   webhooks must have ``failurePolicy: Ignore``.

   Note carefully, because it is counter-intuitive: removing ``pod`` from
   ``integrations.frameworks`` does **not** unregister ``mpod``/``vpod``. The
   upstream chart renders those webhooks UNCONDITIONALLY and uses the framework
   list only to switch ``failurePolicy`` between ``Fail`` and ``Ignore``
   (``templates/webhook/manifests.yaml``: ``{{- if has "pod" ... }}
   failurePolicy: Fail {{- else }} failurePolicy: Ignore {{- end }}``). There is
   no values hook that removes them. ``Ignore`` is therefore both the
   render-visible proof that Djinn disabled the framework AND the actual
   availability guarantee: an unreachable Kueue webhook is skipped rather than
   fatal. If someone puts ``pod`` back, this assertion fires.

   ``jobs`` is deliberately excluded from the ``Ignore`` requirement: batch/job
   is the framework Djinn will use at cutover, so ``mjob``/``vjob`` are
   legitimately ``Fail``. Assertion 1 is what keeps that safe.

WHAT IS NO LONGER CHECKED, AND WHY — READ THIS BEFORE TRUSTING THE PASS
----------------------------------------------------------------------
The retired fork also required a SECOND, per-object fence on those webhooks:

    objectSelector:
      matchLabels:
        djinn.io/kueue-build-object: "true"

**The upstream chart has no objectSelector hook of any kind.** There is no
value, at any version, that injects one. The only ways to keep that assertion
would be to re-fork the upstream manifest or to post-process the rendered
output — both of which recreate precisely the maintenance problem that pinning
an upstream chart exists to remove. So the assertion is REMOVED, not weakened
and not silently reinterpreted.

The resulting scope reduction, which is an input to cutover epic 4c9q:

  * ``mpod``/``vpod``, ``mdeployment``/``vdeployment`` and
    ``mstatefulset``/``vstatefulset`` still register on core CREATE, but with
    ``failurePolicy: Ignore``, so a Kueue outage cannot block those creations.
  * ``mjob``/``vjob`` keep upstream's ``failurePolicy: Fail`` and are now
    fenced by NAMESPACE ONLY. In a namespace labelled
    ``djinn.io/kueue-managed=true``, *every* batch/v1 Job CREATE is routed
    through Kueue's webhook and a Kueue outage blocks all of them. Under the
    fork, the per-object label bounded that to marked build objects.
  * Today the blast radius is zero because no namespace carries the label —
    asserted directly by check 1 above, not assumed.
  * 4c9q must decide the label's placement with that in mind: a dedicated
    build-Job namespace keeps the fenced set narrow; labelling ``djinn``
    itself would put every Job in the control-plane namespace behind a
    ``Fail``-policy webhook.

Usage:
  check-webhook-selectors.py <rendered-manifest.yaml>
      [--namespace-labels '{"k":"v",...}'] [--namespace-name djinn]
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

try:
    import yaml
except ImportError:  # pragma: no cover - environment defect, not a code path
    print(
        "FAIL: PyYAML is required. This checker parses real `helm template` "
        "output, which a bespoke parser cannot be trusted to read.",
        file=sys.stderr,
    )
    raise SystemExit(2)

WEBHOOK_KINDS = {"MutatingWebhookConfiguration", "ValidatingWebhookConfiguration"}
NAMESPACE_LABEL = "djinn.io/kueue-managed"

# Core Kubernetes types, keyed by the API group that owns them. A webhook is
# "relevant" when it can intercept CREATE on one of these. CRD-backed types
# (ray.io, kubeflow.org, jobset, ...) are out of scope: Djinn creates none of
# them, and their webhooks cannot fire on a cluster without those CRDs.
CORE_TARGETS = {
    "": {"pods"},
    "batch": {"jobs"},
    "apps": {"deployments", "statefulsets"},
}

# Resources whose webhook must prove the framework is disabled. `jobs` is
# deliberately absent: batch/job IS the framework Djinn will use at cutover,
# so its webhook is legitimately armed (failurePolicy: Fail).
MUST_BE_IGNORED = {"pods", "deployments", "statefulsets"}

EXPECTED_SELECTOR = {"matchLabels": {NAMESPACE_LABEL: "true"}}


def selector_matches(selector: Any, labels: dict[str, str]) -> bool:
    """Evaluate a Kubernetes LabelSelector the way the API server does.

    An empty or absent selector matches EVERYTHING — that is the case this
    exists to catch, so it must not be special-cased into a pass.
    """
    if selector is None or selector == {}:
        return True
    if not isinstance(selector, dict):
        # Unparseable: treat as matching. A checker that shrugs at a shape it
        # does not understand is how a fence silently disappears.
        return True
    for key, value in (selector.get("matchLabels") or {}).items():
        if labels.get(key) != value:
            return False
    for expression in selector.get("matchExpressions") or []:
        key = expression.get("key")
        operator = expression.get("operator")
        values = expression.get("values") or []
        present = key in labels
        if operator == "In":
            if not present or labels[key] not in values:
                return False
        elif operator == "NotIn":
            if present and labels[key] in values:
                return False
        elif operator == "Exists":
            if not present:
                return False
        elif operator == "DoesNotExist":
            if present:
                return False
        else:
            raise ValueError(f"unsupported matchExpressions operator: {operator!r}")
    return True


def load(path: Path) -> list[dict[str, Any]]:
    with path.open(encoding="utf-8") as handle:
        return [doc for doc in yaml.safe_load_all(handle) if isinstance(doc, dict)]


def targeted_resources(webhook: dict[str, Any]) -> set[str]:
    """Bare core resource names this webhook can intercept on CREATE."""
    hits: set[str] = set()
    rules = webhook.get("rules")
    if not isinstance(rules, list):
        return hits
    for rule in rules:
        if not isinstance(rule, dict):
            continue
        operations = rule.get("operations") or []
        if not any(operation in {"CREATE", "*"} for operation in operations):
            continue
        groups = rule.get("apiGroups") or []
        resources = rule.get("resources") or []
        for group in groups:
            for owner, owned in CORE_TARGETS.items():
                if group not in {owner, "*"}:
                    continue
                for resource in resources:
                    if not isinstance(resource, str):
                        continue
                    # Strip any subresource: `pods/binding` still reaches pods.
                    bare = resource.split("/", 1)[0]
                    if bare == "*":
                        hits |= owned
                    elif bare in owned:
                        hits.add(bare)
    return hits


def check(
    path: Path,
    namespace_name: str | None = None,
    namespace_labels: dict[str, str] | None = None,
) -> tuple[list[str], int]:
    failures: list[str] = []
    relevant = 0
    documents = load(path)
    configurations = [doc for doc in documents if doc.get("kind") in WEBHOOK_KINDS]
    if not configurations:
        return ([f"{path}: no admission webhook configurations found"], 0)

    for configuration in configurations:
        kind = configuration.get("kind")
        metadata = configuration.get("metadata")
        config_name = (metadata or {}).get("name", "<unnamed>")
        webhooks = configuration.get("webhooks")
        if not isinstance(webhooks, list):
            failures.append(f"{kind}/{config_name}: webhooks is not a list")
            continue
        for webhook in webhooks:
            if not isinstance(webhook, dict):
                continue
            hits = targeted_resources(webhook)
            if not hits:
                continue
            relevant += 1
            name = webhook.get("name", "<unnamed>")
            prefix = f"{kind}/{config_name} webhook {name}"

            selector = webhook.get("namespaceSelector")

            # THE availability assertion: does this webhook actually select the
            # namespace Djinn runs in, as the API server would evaluate it?
            if namespace_labels is not None and selector_matches(selector, namespace_labels):
                failures.append(
                    f"{prefix}: namespaceSelector SELECTS namespace "
                    f"{namespace_name!r} (labels {namespace_labels!r}) for CREATE "
                    f"on {sorted(hits)}. With failurePolicy "
                    f"{webhook.get('failurePolicy')!r} an unavailable Kueue "
                    f"controller would block those creations. selector={selector!r}"
                )

            if selector != EXPECTED_SELECTOR:
                failures.append(
                    f"{prefix}: namespaceSelector must be exactly "
                    f"{EXPECTED_SELECTOR!r} so a namespace is opted IN by label; "
                    f"got {selector!r}"
                )

            gated = hits & MUST_BE_IGNORED
            if gated:
                policy = webhook.get("failurePolicy")
                if policy != "Ignore":
                    failures.append(
                        f"{prefix}: failurePolicy must be 'Ignore' for "
                        f"{sorted(gated)} (upstream sets 'Fail' only when the "
                        f"framework is enabled, so {policy!r} means "
                        f"integrations.frameworks re-enabled it)"
                    )

    if relevant == 0:
        failures.append(
            f"{path}: no core Pod/Job/Deployment/StatefulSet CREATE webhook "
            "rules found — the checker asserted nothing"
        )
    return failures, relevant


def main() -> int:
    parser = argparse.ArgumentParser(description="Kueue admission scoping check")
    parser.add_argument("manifest", type=Path, help="rendered djinn-prereqs manifest")
    parser.add_argument(
        "--namespace-labels",
        help=(
            "JSON object of the labels the djinn chart renders onto its "
            "Namespace. When given, every relevant webhook's namespaceSelector "
            "is evaluated against it and must NOT match."
        ),
    )
    parser.add_argument("--namespace-name", default="djinn")
    args = parser.parse_args()

    labels: dict[str, str] | None = None
    if args.namespace_labels is not None:
        labels = json.loads(args.namespace_labels)
        if not isinstance(labels, dict):
            print("FAIL: --namespace-labels must be a JSON object", file=sys.stderr)
            return 2

    try:
        failures, relevant = check(args.manifest, args.namespace_name, labels)
    except (OSError, ValueError, yaml.YAMLError) as error:
        print(f"FAIL: cannot parse {args.manifest}: {error}", file=sys.stderr)
        return 2
    if failures:
        for failure in failures:
            print(f"FAIL: {failure}", file=sys.stderr)
        return 1
    print(
        f"PASS: {args.manifest}: {relevant} core CREATE webhooks carry the "
        f"positive {NAMESPACE_LABEL} namespace fence"
    )
    if labels is not None:
        print(
            f"PASS: none of those {relevant} webhooks selects namespace "
            f"{args.namespace_name!r} with its real rendered labels {labels}"
        )
    else:
        print(
            "NOTE: --namespace-labels was not supplied, so no webhook was "
            "evaluated against a real namespace."
        )
    print(
        "NOTE: objectSelector is NOT checked and NOT present. The upstream "
        "chart exposes no hook for it; the retired fork's second per-object "
        "fence is gone. Job CREATE in a labelled namespace is namespace-fenced "
        "only. See this file's docstring and deploy/kueue/README.md (4c9q)."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
