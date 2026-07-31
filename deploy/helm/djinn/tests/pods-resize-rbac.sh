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

# ---------------------------------------------------------------------------
# ONE comment-stripping implementation, shared by BOTH python blocks below.
#
# This file used to strip comments for its one BAN and not for any of its
# PRESENCE assertions, which is the worse half of the pair to skip: a ban that
# reads a comment merely false-alarms, but a presence assertion that reads a
# comment goes SILENTLY green after the code it names is deleted. Both blocks
# now import the same helper so the rule cannot be applied to one assertion and
# forgotten on the next.
#
# It is the Python counterpart of `scripts/lib/rust-source-scan.awk`'s `rs_split`
# and is kept deliberately small: this file cannot call awk from inside a Python
# heredoc, and rewriting these guards across languages is a separate job.
# ---------------------------------------------------------------------------
cat >"$WORK/rust_source_text.py" <<'PY'
"""Comment stripping for Rust source-text guards, with its own self-tests."""


def strip_rust_comments(text):
    """Blank out Rust comments, keeping the CODE IN FRONT of a trailing one.

    Byte-for-byte length preserving: comment bodies become spaces (newlines
    kept), so offsets and line numbers in the result index the original file and
    a brace scan over the result stays aligned with it.

    Three properties this has to hold, each of which a real guard got wrong:

      * a trailing comment does NOT launder the code before it. `foo(); // ban`
        is still a call to `foo()`. Dropping the whole line — the cheap fix —
        would let a banned token be smuggled in behind a `//`.
      * a `//` INSIDE a string literal is not a comment. Truncating at the `//`
        of `"https://example"` would blank the rest of a real line of code.
      * string literals are otherwise kept verbatim, because the tokens these
        guards match on (`TOKEN_AUDIENCE: &str = "djinn"`) live inside them.
    """
    out, i, n = [], 0, len(text)
    while i < n:
        char = text[i]
        # A char literal whose value IS a double quote must not open a phantom
        # string and swallow the rest of the file. Replaced by a same-length
        # placeholder so nothing downstream sees the quote.
        if char == "'":
            for width in (3, 4):
                if text[i:i + width] in ("'\"'", "'\\\"'"):
                    out.append("'" + "q" * (width - 2) + "'")
                    i += width
                    break
            else:
                out.append(char)
                i += 1
            continue
        if char == '"':
            out.append(char)
            i += 1
            while i < n:
                if text[i] == "\\":
                    out.append(text[i:i + 2])
                    i += 2
                    continue
                out.append(text[i])
                i += 1
                if text[i - 1] == '"':
                    break
            continue
        if text.startswith("//", i):
            while i < n and text[i] != "\n":
                out.append(" ")
                i += 1
            continue
        if text.startswith("/*", i):
            depth = 1
            out.append("  ")
            i += 2
            while i < n and depth:
                if text.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                    out.append("  ")
                elif text.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                    out.append("  ")
                else:
                    out.append("\n" if text[i] == "\n" else " ")
                    i += 1
            continue
        out.append(char)
        i += 1
    return "".join(out)


def _self_test():
    """Both directions, on fixtures, every run. A stripper nobody tests is the
    same silent hazard as a guard nobody mutates."""
    cases = [
        # (source, must still contain, must no longer contain)
        ('let x = 1; // audience: Some(BAD)', 'let x = 1;', 'audience'),
        ('// audience: Some(BAD)', None, 'audience'),
        ('    /// audience: Some(BAD)', None, 'audience'),
        ('/* audience: Some(BAD) */ let y = 2;', 'let y = 2;', 'audience'),
        ('let u = "https://x/audience"; call();', 'call();', None),
        ('let q = \'"\'; call();', 'call();', None),
    ]
    for source, must_keep, must_drop in cases:
        result = strip_rust_comments(source)
        assert len(result) == len(source), (
            f'strip_rust_comments changed the length of {source!r}: {result!r}')
        if must_keep is not None:
            assert must_keep in result, (
                f'strip_rust_comments ate code: {source!r} -> {result!r}')
        if must_drop is not None:
            assert must_drop not in result, (
                f'strip_rust_comments left a comment behind: {source!r} -> {result!r}')
    # The string-literal body survives: these guards match tokens inside one.
    assert '"djinn"' in strip_rust_comments('const A: &str = "djinn"; // note')


_self_test()
PY
export PYTHONPATH="$WORK${PYTHONPATH:+:$PYTHONPATH}"

helm template pods-resize-rbac "$CHART_DIR" --is-upgrade >"$WORK/render.yaml"

python3 - "$WORK/render.yaml" "$JOB_RS" <<'PY'
import copy
import re
import sys
from pathlib import Path

import yaml

from rust_source_text import strip_rust_comments

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


def pod_spec_block(code):
    """Return the task-run PodSpec struct literal, out of ALREADY-STRIPPED code.

    Scoped by brace matching rather than grepping the whole file so the
    assertion below reads the PodSpec djinn-server actually dispatches, not a
    doc comment, a test fixture, or some other struct that happens to mention
    the field.

    The caller strips comments BEFORE this runs, and that ordering is the point:
    locating the marker in raw text let a commented-out
    `// let pod_spec = PodSpec {` earlier in the file capture the whole block
    scope, and brace-matching over raw text let a `{` inside a doc comment
    unbalance the count. Either one silently redirects every assertion below to
    a region of the file that is not the PodSpec.
    """
    marker = 'let pod_spec = PodSpec {'
    assert marker in code, 'task-run PodSpec literal not found in job.rs'
    start = code.index(marker)
    cursor = start + len(marker)
    depth = 1
    while depth:
        assert cursor < len(code), 'unbalanced braces in the task-run PodSpec literal'
        if code[cursor] == '{':
            depth += 1
        elif code[cursor] == '}':
            depth -= 1
        cursor += 1
    return code[start:cursor]


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
    # EVERY source-text assertion below reads `code`, never `job_rs`. A comment
    # is prose: it can neither satisfy a presence assertion nor trip a ban.
    code = strip_rust_comments(job_rs)
    spec = pod_spec_block(code)
    assert re.search(r'automount_service_account_token:\s*Some\(false\)', spec), (
        'the task-run PodSpec no longer sets automount_service_account_token: Some(false); '
        'the apiserver-capable default ServiceAccount token would be projected into a Pod '
        'running repository-controlled code'
    )
    stray = re.search(r'automount_service_account_token:\s*(Some\(true\)|None)', code)
    assert not stray, (
        f'job.rs sets automount_service_account_token: {stray.group(1)} somewhere; '
        'the task-run Pod must never automount the apiserver token'
    )
    audience = re.search(r'pub const TOKEN_AUDIENCE: &str = "([^"]*)";', code)
    assert audience, 'job.rs no longer declares TOKEN_AUDIENCE'
    assert audience.group(1) == EXPECTED_AUDIENCE, (
        f'the task-run projected token audience is {audience.group(1)!r}, expected '
        f'{EXPECTED_AUDIENCE!r}. An apiserver (or empty) audience would make the one token '
        'task-run Pods do hold usable against the apiserver'
    )
    assert re.search(r'audience:\s*Some\(TOKEN_AUDIENCE\.to_string\(\)\)', code), (
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


def expect_accepted(label, mutant_docs, mutant_job):
    """The other direction: prose about a token is not the token.

    A guard that only ever proves it REJECTS things drifts into rejecting the
    comment that explains why the token is wrong. That failure is loud rather
    than silent, but it is still a guard nobody can edit around, so it gets the
    same fixture treatment.
    """
    try:
        validate(mutant_docs, mutant_job)
    except AssertionError as error:
        raise AssertionError(f'{label}: a COMMENT was read as code: {error}') from None
    print(f'  comment-blindness OK: {label}')


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

# ---------------------------------------------------------------------------
# (e) COMMENT BLINDNESS, both directions, for every source-text assertion above.
#
# A BAN and a PRESENCE assertion need DIFFERENT mutations, and neither proves
# anything about the other:
#   * to exercise a PRESENCE arm you REMOVE (or comment out) the required token;
#   * to exercise a BAN you ADD the banned token while LEAVING the required one
#     in place — otherwise you are re-testing the presence arm and reporting it
#     as ban coverage.
# Both arms appear below for each assertion.
# ---------------------------------------------------------------------------

# (e1) PRESENCE — the defect this section was written for. Delete the real
# projected-token binding and leave a doc comment that still mentions it. Read
# against raw source this stayed GREEN while the invariant was false in
# production, which is the silent direction.
expect_rejected(
    'the audience binding commented out, comment left behind',
    docs,
    job_rs.replace('audience: Some(TOKEN_AUDIENCE.to_string()),',
                   '// audience: Some(TOKEN_AUDIENCE.to_string()),'),
    'no longer binds its audience to TOKEN_AUDIENCE',
)

# (e2) PRESENCE — the same for the constant itself.
expect_rejected(
    'the TOKEN_AUDIENCE declaration commented out',
    docs,
    job_rs.replace('pub const TOKEN_AUDIENCE: &str = "djinn";',
                   '// pub const TOKEN_AUDIENCE: &str = "djinn";'),
    'no longer declares TOKEN_AUDIENCE',
)

# (e3) PRESENCE, false-POSITIVE arm — a commented-out DECOY declaration ahead of
# the real one must not be the match. Raw-text `re.search` takes the first hit,
# so this reported an apiserver audience against a healthy file.
expect_accepted(
    'a commented-out decoy TOKEN_AUDIENCE ahead of the real one',
    docs,
    job_rs.replace('pub const TOKEN_AUDIENCE: &str = "djinn";',
                   '// pub const TOKEN_AUDIENCE: &str = "kubernetes.default.svc";\n'
                   'pub const TOKEN_AUDIENCE: &str = "djinn";'),
)

# (e4) BAN, false-POSITIVE arm — prose explaining why the banned value is wrong
# must not fire the ban. The required Some(false) is untouched, so this is the
# ban arm and not the presence arm.
expect_accepted(
    'a comment explaining why automount_service_account_token: Some(true) is wrong',
    docs,
    job_rs + '\n// never write automount_service_account_token: Some(true) here\n',
)

# (e5) BAN — a trailing comment must NOT launder the code in front of it.
# `foo(); // banned` still calls foo(). Dropping the whole line would be the
# cheap way to strip comments and would smuggle this straight through.
expect_rejected(
    'a banned token laundered by a trailing comment',
    docs,
    job_rs + '\n    automount_service_account_token: Some(true), // deliberate, honest\n',
    'somewhere; the task-run Pod must never automount',
)

# (e6) BAN — a `//` inside a string literal is not a comment. Truncating there
# would blank the rest of a REAL line of code and hide the token after it.
expect_rejected(
    'a banned token after a string literal containing //',
    docs,
    job_rs + '\n    let _doc = "https://example.invalid/x"; '
             'automount_service_account_token: Some(true),\n',
    'somewhere; the task-run Pod must never automount',
)

# (e7) SCOPE — `pod_spec_block` located its marker in raw text, so a commented-out
# `let pod_spec = PodSpec {` earlier in the file captured the block scope and
# every assertion inside it then read the wrong region.
DECOY_POD_SPEC = (
    '    // let pod_spec = PodSpec {\n'
    '    //     automount_service_account_token: Some(true),\n'
    '    // };\n'
)
expect_accepted(
    'a commented-out PodSpec literal ahead of the real one',
    docs,
    job_rs.replace('    let pod_spec = PodSpec {', DECOY_POD_SPEC + '    let pod_spec = PodSpec {', 1),
)
# ...and the same text as CODE is still refused, so (e7) is not just "the guard
# ignores everything that looks like this".
expect_rejected(
    'a second, automounting PodSpec literal ahead of the real one',
    docs,
    job_rs.replace('    let pod_spec = PodSpec {',
                   '    let _decoy = PodSpec {\n'
                   '        automount_service_account_token: Some(true),\n'
                   '    };\n'
                   '    let pod_spec = PodSpec {', 1),
    'somewhere; the task-run Pod must never automount',
)

print('PASS: pods/resize is granted once, namespaced, patch-only, and the '
      'task-run ServiceAccount holds no apiserver credential')
PY

# ---------------------------------------------------------------------------
# zpen (proposal 3i92): the couplings the cutover preflight depends on.
# ---------------------------------------------------------------------------
# `server/crates/djinn-k8s/src/cutover_preflight.rs` re-asks the questions above
# at cutover time, against a live render. Two things it relies on live in THIS
# chart, and if either drifts the preflight does not fail — it passes VACUOUSLY,
# which is strictly worse than not existing:
#
#   1. The triple it matches (`RESIZE_RULE_API_GROUP` / `_RESOURCE` / `_VERB`)
#      must be the triple this render actually grants. If the chart moved to a
#      different apiGroup or verb, the preflight would report "no Role grants
#      pods/resize" on a healthy cluster — or a matching-but-wrong constant
#      would report a grant that does not exist.
#   2. The preflight names the task-run ServiceAccount by reading it back off
#      the render's `app.kubernetes.io/component: taskrun` label, deliberately
#      rather than hard-coding it, so a renamed account is still checked. If
#      that label disappeared, the preflight's binding scan would have no
#      account to compare subjects against and would pass with nothing checked.
#
# Asserted here rather than only in the Rust suite because this is a property OF
# THE CHART, and this file is where a chart edit is reviewed.
python3 - "$WORK/render.yaml" "$REPO_DIR/server/crates/djinn-k8s/src/cutover_preflight.rs" <<'PY'
import re
import sys
from pathlib import Path

import yaml

from rust_source_text import strip_rust_comments

render, cutover_rs = (Path(p) for p in sys.argv[1:3])
docs = [d for d in yaml.safe_load_all(render.read_text(encoding='utf-8')) if isinstance(d, dict)]
# Same rule as the block above, same helper: a commented-out or decoy constant is
# prose. Reading the raw file meant a `// pub const RESIZE_RULE_VERB: &str =
# "get";` above the real declaration decided what this guard compared the render
# against, and a declaration deleted but left in a comment kept it green.
raw_source = cutover_rs.read_text(encoding='utf-8')
source = strip_rust_comments(raw_source)
failures = []


def constant(name, text=None):
    match = re.search(rf'pub const {name}: &str = "([^"]*)";', source if text is None else text)
    assert match, f'{name} is not declared in cutover_preflight.rs'
    return match.group(1)


triple = (constant('RESIZE_RULE_API_GROUP'),
          constant('RESIZE_RULE_RESOURCE'),
          constant('RESIZE_RULE_VERB'))


def rules_matching(documents, want):
    api_group, resource, verb = want
    return [
        (doc, rule)
        for doc in documents
        if doc.get('kind') in ('Role', 'ClusterRole')
        for rule in doc.get('rules') or []
        if api_group in (rule.get('apiGroups') or [])
        and resource in (rule.get('resources') or [])
        and verb in (rule.get('verbs') or [])
    ]


def taskrun_accounts(documents):
    return [
        doc['metadata']['name']
        for doc in documents
        if doc.get('kind') == 'ServiceAccount'
        and ((doc.get('metadata') or {}).get('labels') or {})
        .get('app.kubernetes.io/component') == 'taskrun'
    ]


def check(documents, want):
    """The two couplings as one verdict, so a mutation can flip it."""
    problems = []
    matches = rules_matching(documents, want)
    if len(matches) != 1:
        problems.append(
            f'the cutover preflight matches the triple apiGroups={want[0]!r} '
            f'resources={want[1]!r} verbs={want[2]!r}, which this render grants '
            f'{len(matches)} times (expected exactly 1)')
    elif matches[0][0].get('kind') != 'Role':
        problems.append('the granting object is not a namespaced Role')
    accounts = taskrun_accounts(documents)
    if len(accounts) != 1:
        problems.append(
            f'the cutover preflight names the task-run ServiceAccount by its '
            f'app.kubernetes.io/component=taskrun label; this render carries '
            f'{len(accounts)} such accounts (expected exactly 1)')
    return problems


failures.extend(check(docs, triple))

# Non-vacuity, coupling 1: a constant that no longer names what the chart grants
# must be caught here rather than surface as a phantom missing rule at cutover.
if not check(docs, ('apps', triple[1], triple[2])):
    failures.append('a drifted apiGroup constant was not detected')
if not check(docs, (triple[0], triple[1], 'get')):
    failures.append('a drifted verb constant was not detected')
# The RESOURCE constant cannot be proved by a render match, and pretending
# otherwise would be the vacuous assertion this whole file exists to forbid:
# the chart legitimately grants apiGroups [""] / resources ["pods"] /
# verbs ["patch"] on the line above the resize rule, so a constant that drifted
# from `pods/resize` to `pods` would still find exactly one matching Role rule.
# What IS provable is that the constant names a SUBRESOURCE at all — RBAC does
# not imply subresources, so `pods` grants nothing on `pods/resize`.
if not triple[1].startswith('pods/'):
    failures.append(
        f'RESIZE_RULE_RESOURCE is {triple[1]!r}, which is not a pods subresource; '
        f'RBAC on `pods` grants nothing on `pods/resize`')

# Non-vacuity, coupling 2: strip the label the preflight keys on.
stripped = [
    {**doc, 'metadata': {**doc['metadata'],
                         'labels': {k: v
                                    for k, v in (doc['metadata'].get('labels') or {}).items()
                                    if k != 'app.kubernetes.io/component'}}}
    if doc.get('kind') == 'ServiceAccount' else doc
    for doc in docs
]
if not check(stripped, triple):
    failures.append('removing the component=taskrun label was not detected')

# Non-vacuity, comment blindness: `constant()` reads a DECLARATION, not prose
# about one. Both directions, because they fail differently:
#   * a decoy in a comment ahead of the real declaration must be ignored
#     (otherwise this guard compares the render against a string nobody ships);
#   * a declaration that has been deleted and left behind as a comment must NOT
#     satisfy the lookup (otherwise the guard is green with nothing declared).
DECOY = ('// pub const RESIZE_RULE_VERB: &str = "get";\n'
         'pub const RESIZE_RULE_VERB: &str = "patch";')
decoyed = strip_rust_comments(
    raw_source.replace('pub const RESIZE_RULE_VERB: &str = "patch";', DECOY, 1))
if constant('RESIZE_RULE_VERB', decoyed) != triple[2]:
    failures.append(
        'a commented-out decoy declaration was read as RESIZE_RULE_VERB; the '
        'preflight would be compared against a constant the crate does not declare')

commented_out = strip_rust_comments(
    raw_source.replace('pub const RESIZE_RULE_VERB: &str = "patch";',
                       '// pub const RESIZE_RULE_VERB: &str = "patch";', 1))
try:
    constant('RESIZE_RULE_VERB', commented_out)
except AssertionError:
    pass
else:
    failures.append(
        'a commented-out RESIZE_RULE_VERB still satisfied the declaration lookup; '
        'this check would stay green after the constant was deleted')

if failures:
    for failure in failures:
        print(f'FAIL: {failure}', file=sys.stderr)
    raise SystemExit(1)

print('PASS: the cutover preflight matches the triple this chart grants, and can '
      'name the task-run ServiceAccount from the render')
PY
