#!/usr/bin/env bash
# Helm rendering test for the BuildKit registry cache (`cache/*`) retention
# policy, and for the honesty of the declared PVC sizes.
#
# Companion to zot-retention-render.sh, which owns the `djinn-image-*` CATALOG
# policy. The two repo families grow differently and therefore need different
# policy shapes:
#
#   djinn-image-*  one new TAG per content hash → newest-N tag retention.
#   cache/*        `--export-cache ...,mode=max` rewrites the SAME `:latest`
#                  ref every build, so tag count is pinned at one and newest-N
#                  is a no-op. What accumulates is the superseded, untagged
#                  manifest, which keeps its blobs reachable — which is exactly
#                  why `gc: true` reclaimed 0.0 GiB in production. The operator
#                  that produces the garbage is `deleteUntagged`.
#
# Usage: bash deploy/helm/djinn/tests/zot-cache-retention-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

require_tool() {
    if ! command -v "$1" &>/dev/null; then
        echo "FAIL: required test tool '$1' is not installed" >&2
        exit 1
    fi
}

require_tool helm
require_tool python3

TMPDIR_RENDER=$(mktemp -d)
trap 'rm -rf "$TMPDIR_RENDER"' EXIT

# Render the Zot ConfigMap for an in-cluster-Zot topology.
render_zot_config() {
    local output=$1
    shift

    helm template test-release "$CHART_DIR" \
        --is-upgrade \
        --show-only templates/zot-configmap.yaml \
        --set imagePipeline.enabled=true \
        --set imagePipeline.zot.enabled=true \
        "$@" \
        > "$output"
}

# Shared helper: pull `data.config.json` out of a rendered ConfigMap without
# requiring a third-party YAML parser. Written to a real module on PYTHONPATH so
# every checker below can use a QUOTED heredoc — an unquoted one would let the
# shell perform command substitution on backticks inside the Python comments.
cat > "$TMPDIR_RENDER/zotcfg.py" <<'PY'
import json


def load_zot_config(path):
    rendered = open(path, encoding="utf-8").read().splitlines()
    try:
        start = rendered.index("  config.json: |") + 1
    except ValueError:
        raise AssertionError("zot ConfigMap data.config.json was not rendered")
    lines = []
    for line in rendered[start:]:
        if line and not line.startswith("    "):
            break
        lines.append(line[4:] if line else "")
    return json.loads("\n".join(lines))
PY
export PYTHONPATH="$TMPDIR_RENDER"

echo "=== Test 1: shipped defaults render a cache/* policy alongside the catalog one ==="
# No retention.* override at all: this is what an operator who installs the
# chart and reads nothing actually gets.
render_zot_config "$TMPDIR_RENDER/shipped-defaults.yaml"

python3 - "$TMPDIR_RENDER/shipped-defaults.yaml" <<'PY'
from zotcfg import load_zot_config

import sys

config = load_zot_config(sys.argv[1])
retention = config["storage"]["retention"]
policies = retention["policies"]

# The catalog policy must survive untouched — this test must not silently
# unpick the policy the catalog rollout depends on.
catalog = [p for p in policies if p["repositories"] == ["djinn-image-*"]]
assert len(catalog) == 1, f"catalog policy missing or duplicated: {policies}"
assert policies[0] is catalog[0], "catalog policy must stay first"

cache = [p for p in policies if p["repositories"] == ["cache/*"]]
assert len(cache) == 1, (
    "shipped defaults must render exactly one ['cache/*'] policy; without it the "
    f"BuildKit cache repos have no retention, no TTL and no eviction. Got {policies}"
)
cache = cache[0]

# deleteUntagged is the whole point: it is the only operator that turns a
# superseded cache manifest into garbage that gc can then reclaim.
assert cache["deleteUntagged"] is True, (
    "cache policy must set deleteUntagged: it is the only setting that targets "
    "the actual growth vector (superseded, still-indexed manifests). Without it "
    "the policy renders but bounds nothing — gc already measured 0.0 GiB."
)

# Non-destructive by default, inherited from the shared retention-level flag.
assert retention["dryRun"] is True, (
    "shipped default must stay report-only; Zot's dryRun is retention-level and "
    "governs the cache policy too"
)
print("PASS: shipped defaults render a report-only cache/* policy with deleteUntagged")
PY

echo ""
echo "=== Test 2: the catch-all keepTags rule protects the live :latest ref ==="
# This is the assertion that would stay green if the body did nothing, unless
# it is written against the property that matters. The property: an in-flight
# build about to `--import-cache <reg>/cache/<subject>` must never race a policy
# that could select that live ref. A catch-all keepTags rule makes the live ref
# STRUCTURALLY ineligible. Any narrowing qualifier (pushedWithin / pulledWithin /
# newest) reintroduces an expiry and breaks that guarantee.
python3 - "$TMPDIR_RENDER/shipped-defaults.yaml" <<'PY'
from zotcfg import load_zot_config

import re
import sys

config = load_zot_config(sys.argv[1])
cache = next(p for p in config["storage"]["retention"]["policies"]
             if p["repositories"] == ["cache/*"])

keep = cache.get("keepTags")
assert isinstance(keep, list) and keep, (
    "cache policy must carry an explicit keepTags list. An empty/absent list "
    "leaves tag disposition up to Zot's default, which is not a property this "
    "chart may assume for a live cache ref."
)

catch_all = [r for r in keep
             if any(re.fullmatch(p, "any-live-cache-tag") for p in r.get("patterns", []))]
assert catch_all, (
    f"no keepTags rule matches an arbitrary live tag; keepTags={keep}. The live "
    "`:latest` ref would become a deletion candidate and a mid-flight "
    "--import-cache could lose its cache."
)
for rule in catch_all:
    for narrowing in ("pushedWithin", "pulledWithin", "newest"):
        assert narrowing not in rule, (
            f"the catch-all keepTags rule carries {narrowing!r}, which re-adds an "
            "expiry to the live cache ref. The retention guarantee here is "
            "'retain every tag, collect superseded manifests' — nothing weaker."
        )
print("PASS: cache policy retains every tag unconditionally; only superseded manifests are candidates")
PY

echo ""
echo "=== Test 3: the two globs are disjoint over the repo names this system produces ==="
# Zot compiles repository patterns with '/' as the glob separator, so `*` does
# not cross a path segment. Prove the globs against the ACTUAL repo names the
# image controller emits, rather than trusting that they look different:
#   catalog: `<registry>/djinn-image-<sanitized-id>`   (controller.rs)
#   project: `<registry>/djinn-project-<sanitized-id>` (controller.rs)
#   cache:   `<registry>/cache/<sanitized-id>`         (build_job.rs)
# `sanitize_id` maps '/' to '-', so a cache repo is always exactly two segments.
python3 - "$TMPDIR_RENDER/shipped-defaults.yaml" <<'PY'
from zotcfg import load_zot_config

import re
import sys

config = load_zot_config(sys.argv[1])
policies = config["storage"]["retention"]["policies"]


def glob_to_re(pattern):
    """gobwas/glob semantics with '/' as separator: ** crosses, * does not."""
    out, i = "", 0
    while i < len(pattern):
        if pattern.startswith("**", i):
            out += ".*"
            i += 2
        elif pattern[i] == "*":
            out += "[^/]*"
            i += 1
        elif pattern[i] == "?":
            out += "[^/]"
            i += 1
        else:
            out += re.escape(pattern[i])
            i += 1
    return re.compile(out + r"\Z")


def matches(policy, repo):
    return any(glob_to_re(g).match(repo) for g in policy["repositories"])


catalog = next(p for p in policies if p["repositories"] == ["djinn-image-*"])
cache = next(p for p in policies if p["repositories"] == ["cache/*"])

cases = [
    # repo name,                     catalog?, cache?
    ("djinn-image-019e9907-img",     True,     False),
    ("cache/019e9907-img",           False,    True),
    ("cache/proj-abc",               False,    True),
    # djinn-project-* is a THIRD repo family (format_image_tag) and is
    # deliberately matched by NEITHER policy — see the PR body. Assert the
    # current state explicitly so it cannot drift in unnoticed.
    ("djinn-project-019e51db-proj",  False,    False),
    # '*' must not cross '/': a hypothetical nested repo is out of scope, and
    # so is the bare `cache` repo name.
    ("cache/a/b",                    False,    False),
    ("cache",                        False,    False),
]
for repo, want_catalog, want_cache in cases:
    assert matches(catalog, repo) is want_catalog, (
        f"catalog glob match for {repo!r} should be {want_catalog}"
    )
    assert matches(cache, repo) is want_cache, (
        f"cache glob match for {repo!r} should be {want_cache}"
    )

# Belt and braces: no repo name may be claimed by both policies. Zot applies
# the first matching policy, so an overlap would silently apply newest-N tag
# pruning to a cache repo.
for repo, _, _ in cases:
    assert not (matches(catalog, repo) and matches(cache, repo)), (
        f"{repo!r} matches BOTH policies; the first-match rule would apply the "
        "wrong policy shape to it"
    )
print("PASS: catalog and cache globs are disjoint over the real repo names")
PY

echo ""
echo "=== Test 4: cache.enabled=false removes the cache policy and nothing else ==="
render_zot_config "$TMPDIR_RENDER/cache-off.yaml" \
    --set imagePipeline.zot.retention.cache.enabled=false
python3 - "$TMPDIR_RENDER/cache-off.yaml" <<'PY'
from zotcfg import load_zot_config

import sys

config = load_zot_config(sys.argv[1])
policies = config["storage"]["retention"]["policies"]
assert len(policies) == 1, f"expected only the catalog policy, got {policies}"
assert policies[0]["repositories"] == ["djinn-image-*"], (
    f"disabling the cache policy must leave the catalog policy intact, got {policies}"
)
print("PASS: cache.enabled=false renders the catalog policy alone")
PY
if grep -Fq '"cache/' "$TMPDIR_RENDER/cache-off.yaml"; then
    echo "FAIL: cache glob still present with cache.enabled=false" >&2
    exit 1
fi

echo ""
echo "=== Test 5: the master switch still governs the cache policy ==="
# retention.enabled=false must remove the WHOLE retention block. A cache policy
# that leaked out from under the master switch would be a policy an operator
# who disabled retention did not consent to.
render_zot_config "$TMPDIR_RENDER/retention-off.yaml" \
    --set imagePipeline.zot.retention.enabled=false
python3 - "$TMPDIR_RENDER/retention-off.yaml" <<'PY'
from zotcfg import load_zot_config

import sys

config = load_zot_config(sys.argv[1])
assert "retention" not in config["storage"], (
    "retention.enabled=false must render no retention block at all, got "
    f"{config['storage'].get('retention')}"
)
print("PASS: retention.enabled=false removes both policies")
PY

echo ""
echo "=== Test 6: destructive mode renders both policies ==="
render_zot_config "$TMPDIR_RENDER/destructive.yaml" \
    --set imagePipeline.zot.retention.dryRun=false
python3 - "$TMPDIR_RENDER/destructive.yaml" <<'PY'
from zotcfg import load_zot_config

import sys

config = load_zot_config(sys.argv[1])
retention = config["storage"]["retention"]
assert retention["dryRun"] is False, "dryRun=false must reach the rendered policy"
globs = [g for p in retention["policies"] for g in p["repositories"]]
assert globs == ["djinn-image-*", "cache/*"], (
    f"destructive mode must carry both policies, in catalog-first order; got {globs}"
)
# Zot's dryRun is retention-level, not per-policy: the operator opt-in is a
# single action covering both. Assert no per-policy dryRun key was invented,
# because Zot would silently ignore it and the safety story would be a fiction.
for policy in retention["policies"]:
    assert "dryRun" not in policy, (
        "per-policy dryRun is not a Zot setting; it would be silently ignored"
    )
print("PASS: destructive mode renders catalog + cache policies under one shared dryRun")
PY

echo ""
echo "=== Test 7: external-registry topology stays inert (regression guard) ==="
# The server's startup preflight is fail-closed and its caller exit(1)s on a Zot
# fetch error. With the chart default `zot.enabled: false` there is no Zot
# ConfigMap and no auth Secret, so the effective retention flag must stay false.
# Adding a second policy must not have disturbed that.
if helm template test-release "$CHART_DIR" \
    --is-upgrade \
    --show-only templates/zot-configmap.yaml \
    > "$TMPDIR_RENDER/no-zot-cm.yaml" 2>&1; then
    echo "FAIL: a Zot ConfigMap rendered with the default zot.enabled=false" >&2
    exit 1
fi

helm template test-release "$CHART_DIR" \
    --is-upgrade \
    --show-only templates/deployment-server.yaml \
    > "$TMPDIR_RENDER/no-zot-server.yaml"
python3 - "$TMPDIR_RENDER/no-zot-server.yaml" <<'PY'
import re
import sys

rendered = open(sys.argv[1], encoding="utf-8").read().splitlines()
start = next(i for i, line in enumerate(rendered)
             if line == "            - name: DJINN_ZOT_RETENTION_ENABLED")
end = next((i for i in range(start + 1, len(rendered))
            if rendered[i].startswith("            - name: ")), len(rendered))
match = re.search(r"^              value: (.+)$", "\n".join(rendered[start:end]), re.MULTILINE)
assert match and match.group(1).strip('"') == "false", (
    "effective retention must stay false without an in-cluster Zot, or the "
    "fail-closed startup preflight exit(1)s the server on boot"
)
print("PASS: external-registry default still renders effective retention=false")
PY

echo ""
echo "=== Test 8: declared PVC sizes are the documented ones ==="
# Problem 2. Under the k3s/kind `local-path` provisioner these numbers enforce
# NOTHING — production runs 40Gi-declared PVCs holding 83G each. They are kept
# here (rather than raised to match observed usage) because raising a request on
# a non-expandable StorageClass makes `helm upgrade` fail outright, and because
# the real bound is on the producers, not the volume. This assertion exists so
# the numbers cannot drift away from what values.yaml and docs/deploy/vps.md say
# about them.
helm template test-release "$CHART_DIR" \
    --is-upgrade \
    --show-only templates/zot-pvc.yaml \
    --show-only templates/pvc-cache.yaml \
    --show-only templates/pvc-mirror.yaml \
    --set imagePipeline.enabled=true \
    --set imagePipeline.zot.enabled=true \
    > "$TMPDIR_RENDER/pvcs.yaml"
python3 - "$TMPDIR_RENDER/pvcs.yaml" <<'PY'
import re
import sys

rendered = open(sys.argv[1], encoding="utf-8").read()
sizes = {}
for doc in rendered.split("\n---\n"):
    name = re.search(r"^  name: (\S+)$", doc, re.MULTILINE)
    storage = re.search(r"^      storage: (\S+)$", doc, re.MULTILINE)
    if name and storage:
        sizes[name.group(1)] = storage.group(1)

expected = {
    "test-release-djinn-zot": "100Gi",
    "test-release-djinn-cache": "20Gi",
    "test-release-djinn-mirrors": "50Gi",
}
for pvc, want in expected.items():
    assert sizes.get(pvc) == want, (
        f"{pvc} should request {want}, got {sizes.get(pvc)!r}. If this changed "
        "deliberately, update values.yaml's storage preamble and "
        "docs/deploy/vps.md together — and check the StorageClass allows volume "
        "expansion, because local-path does not and helm upgrade will be rejected."
    )
print(f"PASS: declared PVC requests match the documented defaults: {sizes}")
PY

echo ""
echo "=== All BuildKit cache retention rendering tests passed ==="
