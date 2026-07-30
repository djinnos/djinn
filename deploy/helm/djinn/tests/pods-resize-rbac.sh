#!/usr/bin/env bash
# Chart contract for the `pods/resize` RBAC grant and the task-run credential
# boundary it depends on (i6ug / proposal 3i92's CPU quota cutover).
#
# The property under test is not "a resize rule exists". It is: djinn-server,
# holding the controller ServiceAccount, is the ONLY actor in the cluster that
# can call `PATCH pods/resize`, and it can do so only inside its own namespace.
# Four facts have to hold together for that to be true, so they are asserted
# together here — an RBAC edit cannot silently take the credential boundary
# with it:
#
#   1. exactly one rule in the whole render grants pods/resize, it is exactly
#      apiGroups [""] / resources ["pods/resize"] / verbs ["patch"], and it
#      lives in the NAMESPACED controller Role;
#   2. the task-run ServiceAccount is the subject of no RoleBinding and no
#      ClusterRoleBinding (directly or via a system:serviceaccounts group);
#   3. task-run Pods still render `automountServiceAccountToken: false`, and
#      their one projected token still carries audience `djinn` rather than the
#      apiserver's — so repository-controlled child code holds no apiserver
#      credential to reach the subresource with;
#   4. the render adds no ClusterRole and no ClusterRoleBinding beyond the
#      pre-existing tokenreview pair, which stays djinn-server's only
#      cluster-wide permission.
#
# Exact-Pod authorization is deliberately NOT an RBAC property: Kubernetes RBAC
# cannot express a resourceNames set that only exists at run time (one Pod per
# task-run). It is an application invariant — the durable permit relation plus
# a Pod UID fence. This contract's job is to keep the Role from being widened
# to compensate for that.
#
# Every assertion is proved non-vacuous below: each check is re-run against a
# render (or a job.rs) mutated to violate exactly that check, and must reject
# it for the stated reason.
#
# Usage: bash deploy/helm/djinn/tests/pods-resize-rbac.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# DJINN_CHART_DIR lets a caller point this same contract at a mutated copy of
# the chart, the way deploy/preflight/tests/render-gate.sh does.
CHART_DIR="${DJINN_CHART_DIR:-$(cd "$SCRIPT_DIR/.." && pwd)}"
REPO_DIR="$(cd "$SCRIPT_DIR/../../../.." && pwd)"
JOB_RS="$REPO_DIR/server/crates/djinn-k8s/src/job.rs"

require_tool() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "FAIL: required test tool '$1' is not installed" >&2
    exit 1
  }
}
require_tool helm
require_tool python3

[ -f "$JOB_RS" ] || {
  echo "FAIL: task-run Pod renderer not found at $JOB_RS" >&2
  exit 1
}

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

helm template pods-resize-rbac "$CHART_DIR" --is-upgrade >"$WORK/render.yaml"

python3 - "$WORK/render.yaml" "$JOB_RS" <<'PY'
import copy
import re
import sys
from pathlib import Path

import yaml

RESIZE = 'pods/resize'
BINDING_KINDS = ('RoleBinding', 'ClusterRoleBinding')
ROLE_KINDS = ('Role', 'ClusterRole')
# The one pre-existing cluster-wide permission: TokenReview, which is not a
# namespaced resource, so it cannot be granted any other way.
CLUSTER_SCOPED_ALLOWLIST = '-tokenreview'
EXPECTED_AUDIENCE = 'djinn'


def documents(path):
    return [doc for doc in yaml.safe_load_all(Path(path).read_text(encoding='utf-8')) if doc]


def of_kind(docs, *kinds):
    return [doc for doc in docs if doc.get('kind') in kinds]


def name(doc):
    return doc.get('metadata', {}).get('name', '<unnamed>')


def grants_resize(rule):
    """True if `rule` authorizes any verb on the pods/resize subresource.

    Checked through the wildcards, not by string equality: resources ["*"] in
    the core group reaches pods/resize just as surely as naming it, and a
    contract that only recognised the literal spelling would wave that through.
    """
    groups = rule.get('apiGroups') or []
    resources = rule.get('resources') or []
    if not any(group in ('', '*') for group in groups):
        return False
    return any(resource in (RESIZE, '*', 'pods/*') for resource in resources)


def strip_rust_comments(text):
    return '\n'.join(line for line in text.splitlines() if not line.lstrip().startswith('//'))


def pod_spec_block(text):
    """Return the task-run PodSpec struct literal, comments stripped.

    Scoped by brace matching rather than grepping the whole file so the
    assertion below reads the PodSpec djinn-server actually dispatches, not a
    doc comment, a test fixture, or some other struct that happens to mention
    the field.
    """
    marker = 'let pod_spec = PodSpec {'
    assert marker in text, 'task-run PodSpec literal not found in job.rs'
    start = text.index(marker)
    cursor = start + len(marker)
    depth = 1
    while depth:
        assert cursor < len(text), 'unbalanced braces in the task-run PodSpec literal'
        if text[cursor] == '{':
            depth += 1
        elif text[cursor] == '}':
            depth -= 1
        cursor += 1
    return strip_rust_comments(text[start:cursor])


def validate(docs, job_rs):
    # ---- 1. the grant itself -------------------------------------------------
    grants = [
        (doc, rule)
        for doc in of_kind(docs, *ROLE_KINDS)
        for rule in (doc.get('rules') or [])
        if grants_resize(rule)
    ]
    assert len(grants) == 1, (
        f'expected exactly one rule granting {RESIZE} in the whole render, got {len(grants)}'
        + ''.join(f'\n  {doc["kind"]}/{name(doc)}: {rule}' for doc, rule in grants)
    )
    holder, rule = grants[0]
    assert holder['kind'] == 'Role', (
        f'the {RESIZE} grant must live in a namespaced Role; found it in '
        f'{holder["kind"]}/{name(holder)}. RBAC cannot fence resize to one Pod, so the '
        'namespace is the only fence the Role itself provides — do not promote it'
    )
    assert holder.get('metadata', {}).get('namespace'), (
        f'Role/{name(holder)} carries the {RESIZE} grant without a namespace'
    )
    assert name(holder).endswith('-controller'), (
        f'{RESIZE} is granted by Role/{name(holder)}, not the controller Role'
    )
    assert rule.get('apiGroups') == [''], (
        f'{RESIZE} rule apiGroups must be exactly [""], got {rule.get("apiGroups")!r}'
    )
    assert rule.get('resources') == [RESIZE], (
        f'{RESIZE} rule resources must be exactly ["{RESIZE}"], got {rule.get("resources")!r}'
    )
    assert rule.get('verbs') == ['patch'], (
        f'{RESIZE} rule verbs must be exactly ["patch"], got {rule.get("verbs")!r}. '
        'The resize subresource accepts PATCH only; any other verb is unearned breadth'
    )
    assert set(rule) == {'apiGroups', 'resources', 'verbs'}, (
        f'{RESIZE} rule carries unexpected keys: {sorted(set(rule) - {"apiGroups", "resources", "verbs"})}'
    )

    # No rule anywhere in the render may use an RBAC wildcard. Nothing broader
    # than the enumerated grants is what "and nothing broader" has to mean once
    # a new subresource is in the Role.
    for doc in of_kind(docs, *ROLE_KINDS):
        for entry in doc.get('rules') or []:
            for field in ('apiGroups', 'resources', 'verbs'):
                assert '*' not in (entry.get(field) or []), (
                    f'{doc["kind"]}/{name(doc)} uses an RBAC wildcard in {field}: {entry}'
                )

    # ---- 2. the task-run ServiceAccount holds no RBAC -------------------------
    task_run = [doc for doc in of_kind(docs, 'ServiceAccount') if name(doc).endswith('-taskrun')]
    assert len(task_run) == 1, f'expected exactly one task-run ServiceAccount, got {len(task_run)}'
    task_run_name = name(task_run[0])
    for binding in of_kind(docs, *BINDING_KINDS):
        for subject in binding.get('subjects') or []:
            assert not (
                subject.get('kind') == 'ServiceAccount' and subject.get('name') == task_run_name
            ), (
                f'{binding["kind"]}/{name(binding)} binds the task-run ServiceAccount '
                f'{task_run_name} to {binding.get("roleRef")}. Task-run Pods run '
                'repository-controlled code; they must hold no apiserver authorization at all'
            )
            assert not (
                subject.get('kind') == 'Group'
                and str(subject.get('name', '')).startswith('system:serviceaccounts')
            ), (
                f'{binding["kind"]}/{name(binding)} binds the group {subject.get("name")!r}, '
                'which covers the task-run ServiceAccount'
            )

    # ---- 3. task-run Pods carry no apiserver credential ----------------------
    spec = pod_spec_block(job_rs)
    assert re.search(r'automount_service_account_token:\s*Some\(false\)', spec), (
        'the task-run PodSpec no longer sets automount_service_account_token: Some(false); '
        'the apiserver-capable default ServiceAccount token would be projected into a Pod '
        'running repository-controlled code'
    )
    stray = re.search(r'automount_service_account_token:\s*(Some\(true\)|None)', strip_rust_comments(job_rs))
    assert not stray, (
        f'job.rs sets automount_service_account_token: {stray.group(1)} somewhere; '
        'the task-run Pod must never automount the apiserver token'
    )
    audience = re.search(r'pub const TOKEN_AUDIENCE: &str = "([^"]*)";', job_rs)
    assert audience, 'job.rs no longer declares TOKEN_AUDIENCE'
    assert audience.group(1) == EXPECTED_AUDIENCE, (
        f'the task-run projected token audience is {audience.group(1)!r}, expected '
        f'{EXPECTED_AUDIENCE!r}. An apiserver (or empty) audience would make the one token '
        'task-run Pods do hold usable against the apiserver'
    )
    assert re.search(r'audience:\s*Some\(TOKEN_AUDIENCE\.to_string\(\)\)', job_rs), (
        'the task-run projected token no longer binds its audience to TOKEN_AUDIENCE'
    )

    # ---- 4. nothing new becomes cluster-wide ---------------------------------
    for doc in of_kind(docs, 'ClusterRole', 'ClusterRoleBinding'):
        assert name(doc).endswith(CLUSTER_SCOPED_ALLOWLIST), (
            f'render introduces cluster-scoped RBAC {doc["kind"]}/{name(doc)}. TokenReview '
            'remains the only cluster-wide permission djinn-server may hold; everything '
            'else, resize included, stays namespaced'
        )
    for kind in ('ClusterRole', 'ClusterRoleBinding'):
        found = of_kind(docs, kind)
        assert len(found) == 1, f'expected exactly one {kind} in the render, got {len(found)}'


render_path, job_rs_path = sys.argv[1:3]
docs = documents(render_path)
job_rs = Path(job_rs_path).read_text(encoding='utf-8')

validate(docs, job_rs)


def resize_rule(mutant):
    for doc in of_kind(mutant, *ROLE_KINDS):
        for rule in doc.get('rules') or []:
            if grants_resize(rule):
                return doc, rule
    raise AssertionError('mutation lost the resize rule it meant to edit')


def expect_rejected(label, mutant_docs, mutant_job, needle):
    try:
        validate(mutant_docs, mutant_job)
    except AssertionError as error:
        assert needle in str(error), (
            f'{label}: rejected, but for the wrong reason (wanted {needle!r}): {error}'
        )
        print(f'  non-vacuity OK: {label}')
    else:
        raise AssertionError(f'{label}: forbidden configuration was ACCEPTED')


# (a) AC1 — widen the verbs.
mutant = copy.deepcopy(docs)
resize_rule(mutant)[1]['verbs'] = ['patch', 'update']
expect_rejected('widened verbs to ["patch","update"]', mutant, job_rs, 'verbs must be exactly')

# (a2) AC1 — reach the subresource through a core-group resource wildcard.
mutant = copy.deepcopy(docs)
holder, rule = resize_rule(mutant)
holder['rules'].append({'apiGroups': [''], 'resources': ['*'], 'verbs': ['patch']})
expect_rejected('core-group resource wildcard', mutant, job_rs, 'exactly one rule granting')

# (a3) AC1 — drop the rule entirely; the presence check must fire.
mutant = copy.deepcopy(docs)
holder, rule = resize_rule(mutant)
holder['rules'].remove(rule)
expect_rejected('removed the resize rule', mutant, job_rs, 'exactly one rule granting')

# (b) AC2 — bind the controller Role to the task-run ServiceAccount.
mutant = copy.deepcopy(docs)
task_run_sa = [doc for doc in of_kind(mutant, 'ServiceAccount') if name(doc).endswith('-taskrun')][0]
controller_role = resize_rule(mutant)[0]
mutant.append({
    'apiVersion': 'rbac.authorization.k8s.io/v1',
    'kind': 'RoleBinding',
    'metadata': {'name': 'resize-taskrun', 'namespace': controller_role['metadata']['namespace']},
    'subjects': [{
        'kind': 'ServiceAccount',
        'name': name(task_run_sa),
        'namespace': controller_role['metadata']['namespace'],
    }],
    'roleRef': {
        'apiGroup': 'rbac.authorization.k8s.io',
        'kind': 'Role',
        'name': name(controller_role),
    },
})
expect_rejected('bound the task-run ServiceAccount', mutant, job_rs, 'binds the task-run ServiceAccount')

# (b2) AC2 — reach the same SA through the system:serviceaccounts group.
mutant = copy.deepcopy(docs)
controller_role = resize_rule(mutant)[0]
namespace = controller_role['metadata']['namespace']
mutant.append({
    'apiVersion': 'rbac.authorization.k8s.io/v1',
    'kind': 'RoleBinding',
    'metadata': {'name': 'resize-all-sa', 'namespace': namespace},
    'subjects': [{
        'kind': 'Group',
        'name': f'system:serviceaccounts:{namespace}',
        'apiGroup': 'rbac.authorization.k8s.io',
    }],
    'roleRef': {
        'apiGroup': 'rbac.authorization.k8s.io',
        'kind': 'Role',
        'name': name(controller_role),
    },
})
expect_rejected('bound system:serviceaccounts', mutant, job_rs, 'which covers the task-run ServiceAccount')

# (c) AC3 — stop suppressing the automounted apiserver token.
expect_rejected(
    'automountServiceAccountToken flipped to true',
    docs,
    job_rs.replace('automount_service_account_token: Some(false)',
                   'automount_service_account_token: Some(true)'),
    'no longer sets automount_service_account_token: Some(false)',
)

# (c1b) AC3 — a SECOND PodSpec in the same file that does automount. The
# task-run literal still reads Some(false), so only the file-wide check catches
# this one.
expect_rejected(
    'stray automounting PodSpec elsewhere in job.rs',
    docs,
    job_rs + '\n    automount_service_account_token: Some(true),\n',
    'somewhere; the task-run Pod must never automount',
)

# (c2) AC3 — widen the projected token to an apiserver-valid audience.
expect_rejected(
    'projected token audience emptied',
    docs,
    job_rs.replace('pub const TOKEN_AUDIENCE: &str = "djinn";',
                   'pub const TOKEN_AUDIENCE: &str = "";'),
    'projected token audience is',
)

# (d) AC4 — promote the rule to a ClusterRole.
mutant = copy.deepcopy(docs)
holder, rule = resize_rule(mutant)
holder['rules'].remove(rule)
mutant.append({
    'apiVersion': 'rbac.authorization.k8s.io/v1',
    'kind': 'ClusterRole',
    'metadata': {'name': 'djinn-resize'},
    'rules': [rule],
})
expect_rejected('promoted the rule to a ClusterRole', mutant, job_rs, 'must live in a namespaced Role')

# (d2) AC4 — any other new cluster-scoped object, resize-free, is also refused.
mutant = copy.deepcopy(docs)
mutant.append({
    'apiVersion': 'rbac.authorization.k8s.io/v1',
    'kind': 'ClusterRoleBinding',
    'metadata': {'name': 'djinn-extra'},
    'subjects': [{'kind': 'ServiceAccount', 'name': 'other', 'namespace': 'djinn'}],
    'roleRef': {'apiGroup': 'rbac.authorization.k8s.io', 'kind': 'ClusterRole', 'name': 'cluster-admin'},
})
expect_rejected('added an unrelated ClusterRoleBinding', mutant, job_rs, 'introduces cluster-scoped RBAC')

print('PASS: pods/resize is granted once, namespaced, patch-only, and the '
      'task-run ServiceAccount holds no apiserver credential')
PY
