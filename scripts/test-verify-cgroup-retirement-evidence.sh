#!/bin/sh
# Hermetic fixture suite for verify-cgroup-retirement-evidence.sh.
# It exercises the public shell entry point only; no cluster access is possible.
set -eu
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
CHECKER="$SCRIPT_DIR/verify-cgroup-retirement-evidence.sh"
FIXTURES="$SCRIPT_DIR/fixtures/cgroup-retirement/cases"
SCRATCH=$(mktemp -d /var/tmp/cgroup-retirement-evidence.XXXXXX)
trap 'rm -rf "$SCRATCH"' EXIT INT TERM
PASS=0 FAIL=0
pass() { PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"; }
fail() { FAIL=$((FAIL + 1)); printf '  FAIL %s\n' "$1" >&2; }

run_case() {
    name=$1
    cp -R "$SCRIPT_DIR/fixtures/cgroup-retirement" "$SCRATCH/$name"
    root="$SCRATCH/$name"
    case "$name" in
      malformed) printf '{ not JSON\n' > "$root/candidates/RETIRE_HEAD.json" ;;
      oom) node -e 'let p=process.argv[1],x=require(p);x.runs[5].memory_events_oom_kill_after="13";require("fs").writeFileSync(p,JSON.stringify(x))' "$root/candidates/RETIRE_HEAD.json" ;;
      insufficient-headroom) node -e 'let p=process.argv[1],x=require(p);x.runs[0].ceiling_bytes="2684354559";require("fs").writeFileSync(p,JSON.stringify(x))' "$root/candidates/RETIRE_HEAD.json" ;;
      minimum-margin) node -e 'let p=process.argv[1],x=require(p);x.runs[0].sum_bytes="1073741824";x.runs[0].ceiling_bytes="1610612735";require("fs").writeFileSync(p,JSON.stringify(x))' "$root/candidates/RETIRE_HEAD.json" ;;
      percent-margin) node -e 'let p=process.argv[1],x=require(p);x.runs[0].sum_bytes="10737418240";x.runs[0].ceiling_bytes="12884901887";require("fs").writeFileSync(p,JSON.stringify(x))' "$root/candidates/RETIRE_HEAD.json" ;;
      reservation-mismatch) node -e 'let p=process.argv[1],x=require(p);x.identity_digests.reservation="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";require("fs").writeFileSync(p,JSON.stringify(x))' "$root/candidates/RETIRE_HEAD.json" ;;
      quota-mismatch) node -e 'let p=process.argv[1],x=require(p);x.identity_digests.quota="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";require("fs").writeFileSync(p,JSON.stringify(x))' "$root/candidates/RETIRE_HEAD.json" ;;
      kueue-width-mismatch) node -e 'let p=process.argv[1],x=require(p);x.identity_digests.kueue_width="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";require("fs").writeFileSync(p,JSON.stringify(x))' "$root/candidates/RETIRE_HEAD.json" ;;
      stale-digest) node -e 'let p=process.argv[1],x=require(p);x.identity_digests.evidence="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";require("fs").writeFileSync(p,JSON.stringify(x))' "$root/candidates/RETIRE_HEAD.json" ;;
      lost-fit) node -e 'let p=process.argv[1],x=require(p);x.node_fit.allocatable_bytes="7516192767";require("fs").writeFileSync(p,JSON.stringify(x))' "$root/candidates/RETIRE_HEAD.json" ;;
      duplicate-canary) node -e 'let p=process.argv[1],x=require(p);x.runs[1]={...x.runs[0],role:"canary-2"};require("fs").writeFileSync(p,JSON.stringify(x))' "$root/candidates/RETIRE_HEAD.json" ;;
      missing-field) node -e 'let p=process.argv[1],x=require(p);delete x.runs[0].cgroup_path;require("fs").writeFileSync(p,JSON.stringify(x))' "$root/candidates/RETIRE_HEAD.json" ;;
      unknown-field) node -e 'let p=process.argv[1],x=require(p);x.runs[0].invented="never-default";require("fs").writeFileSync(p,JSON.stringify(x))' "$root/candidates/RETIRE_HEAD.json" ;;
      *) fail "$name has no mutation"; return ;;
    esac
    set +e
    out=$(CGROUP_RETIREMENT_ROOT="$root" "$CHECKER" --candidate RETIRE_HEAD 2>&1)
    code=$?
    set -e
    if [ "$code" -eq 1 ] && printf '%s' "$out" | grep -F "$(cat "$FIXTURES/$name.json" | node -pe 'JSON.parse(require("fs").readFileSync(0,"utf8")).subject')" >/dev/null; then
       pass "$name rejected for its named subject"
    else
       fail "$name (exit $code): $out"
    fi
}

run_landing_case() {
    name=$1
    cp -R "$SCRIPT_DIR/fixtures/cgroup-retirement" "$SCRATCH/landing-$name"
    root="$SCRATCH/landing-$name"
    landing="$root/landing/0123456789abcdef0123456789abcdef01234567.json"
    node - "$landing" "$name" <<'NODE'
const fs=require('fs'), [p,n]=process.argv.slice(2), x=JSON.parse(fs.readFileSync(p)), m='f'.repeat(40);
const outcome=p.replace('.json','.outcome.json');
switch(n) {
case 'no-approval': x.review.approval_state='pending'; break;
case 'changes-requested': x.review.reviews[1].state='changes_requested'; break;
case 'dismissal': x.review.reviews[1].state='dismissed'; break;
case 'stale-head': x.review.reviews[1].head=m; break;
case 'missing-owner': x.review.configured_owners.push('owner-c'); x.review.rule_snapshot.configured_owners.push('owner-c'); break;
case 'rule-mismatch': x.review.rule_snapshot.effective_required_approvals=3; break;
case 'bypass': x.review.no_bypass.direct_push=true; break;
case 'duplicate-approval': x.review.reviews[1].actor=x.review.reviews[0].actor; x.review.configured_owners=['owner-a']; x.review.rule_snapshot.configured_owners=['owner-a']; break;
case 'self-certification': x.review.reviews[0].actor='author'; break;
case 'incomplete-payload': x.review.reviewed_payload.untested_replacements=['installer']; break;
case 'image-digest': x.deployment.image.digest='sha256:'+m+'eeeeeeeeeeeeeeeeeeeeeeee'; break;
case 'render-digest': x.deployment.render_digest.digest='sha256:'+m+'eeeeeeeeeeeeeeeeeeeeeeee'; break;
case 'node-digest': x.deployment.node_digest.digest='sha256:'+m+'eeeeeeeeeeeeeeeeeeeeeeee'; break;
case 'workload-digest': x.deployment.workload_digest.digest='sha256:'+m+'eeeeeeeeeeeeeeeeeeeeeeee'; break;
case 'pod-annotation': x.deployment.pod_annotation.commit=m; break;
case 'dispatch': x.deployment.final_dispatch.container_count=2; break;
case 'proof-failure': {const y=JSON.parse(fs.readFileSync(outcome));y.pre_landing.candidate_proofs='failed';fs.writeFileSync(outcome,JSON.stringify(y));} break;
case 'rollback-fault': {const y=JSON.parse(fs.readFileSync(outcome));y.candidate_fault=true;y.restoration.node_assets='failed';fs.writeFileSync(outcome,JSON.stringify(y));} break;
case 'aggregate-restoration-failed': {const y=JSON.parse(fs.readFileSync(outcome));y.restoration.aggregate_tree='failed';fs.writeFileSync(outcome,JSON.stringify(y));} break;
case 'node-restoration-failed': {const y=JSON.parse(fs.readFileSync(outcome));y.restoration.node_assets='failed';fs.writeFileSync(outcome,JSON.stringify(y));} break;
case 'launcher-restoration-failed': {const y=JSON.parse(fs.readFileSync(outcome));y.restoration.launcher_leaf='failed';fs.writeFileSync(outcome,JSON.stringify(y));} break;
default: throw Error(n);
} fs.writeFileSync(p,JSON.stringify(x));
NODE
    set +e
    out=$(CGROUP_RETIREMENT_ROOT="$root" "$CHECKER" --landing 0123456789abcdef0123456789abcdef01234567 2>&1)
    code=$?
    set -e
    [ "$code" -eq 1 ] && pass "landing $name rejected" || fail "landing $name (exit $code): $out"
}

printf 'Testing immutable cgroup-retirement evidence fixtures\n'
if "$CHECKER" --candidate RETIRE_HEAD >/dev/null; then pass 'positive RETIRE_HEAD fixture'; else fail 'positive RETIRE_HEAD fixture'; fi
for fixture in "$FIXTURES"/*.json; do run_case "$(basename "$fixture" .json)"; done
if "$CHECKER" --landing 0123456789abcdef0123456789abcdef01234567 >/dev/null; then pass 'complete commit-bound landing fixture'; else fail 'complete commit-bound landing fixture'; fi
for case_name in no-approval changes-requested dismissal stale-head missing-owner rule-mismatch bypass duplicate-approval self-certification incomplete-payload image-digest render-digest node-digest workload-digest pod-annotation dispatch proof-failure rollback-fault aggregate-restoration-failed node-restoration-failed launcher-restoration-failed; do run_landing_case "$case_name"; done
out=$(node "$SCRIPT_DIR/cgroup-retirement-outcome.mjs" "$SCRIPT_DIR/fixtures/cgroup-retirement/landing/0123456789abcdef0123456789abcdef01234567.outcome.json")
printf '%s' "$out" | grep -F 'RETIRE one-container-dispatch-authorized' >/dev/null && pass 'all-green state retires' || fail 'all-green state retires'
node - "$SCRIPT_DIR/fixtures/cgroup-retirement/landing/0123456789abcdef0123456789abcdef01234567.outcome.json" "$SCRATCH/keep.json" <<'NODE'
const fs=require('fs'),x=JSON.parse(fs.readFileSync(process.argv[2]));x.pre_landing.review='skipped';fs.writeFileSync(process.argv[3],JSON.stringify(x));
NODE
out=$(node "$SCRIPT_DIR/cgroup-retirement-outcome.mjs" "$SCRATCH/keep.json")
printf '%s' "$out" | grep -F 'KEEP preserved-assets-armed' >/dev/null && pass 'non-green pre-landing state defaults KEEP with assets armed' || fail 'non-green pre-landing state defaults KEEP with assets armed'
node - "$SCRIPT_DIR/fixtures/cgroup-retirement/landing/0123456789abcdef0123456789abcdef01234567.outcome.json" "$SCRATCH/recovery.json" <<'NODE'
const fs=require('fs'),x=JSON.parse(fs.readFileSync(process.argv[2]));x.candidate_fault=true;x.restoration.node_assets='failed';fs.writeFileSync(process.argv[3],JSON.stringify(x));
NODE
out=$(node "$SCRIPT_DIR/cgroup-retirement-outcome.mjs" "$SCRATCH/recovery.json")
printf '%s' "$out" | grep -F 'RECOVERY dispatch-paused' >/dev/null && pass 'fault enters RECOVERY until restoration green' || fail 'fault enters RECOVERY until restoration green'
for proof in aggregate_tree node_assets launcher_leaf; do
    node - "$SCRIPT_DIR/fixtures/cgroup-retirement/landing/0123456789abcdef0123456789abcdef01234567.outcome.json" "$SCRATCH/restoration-$proof.json" "$proof" <<'NODE'
const fs=require('fs'), [source,target,proof]=process.argv.slice(2),x=JSON.parse(fs.readFileSync(source));x.restoration[proof]='failed';fs.writeFileSync(target,JSON.stringify(x));
NODE
    out=$(node "$SCRIPT_DIR/cgroup-retirement-outcome.mjs" "$SCRATCH/restoration-$proof.json")
    printf '%s' "$out" | grep -F 'RECOVERY dispatch-paused' >/dev/null && pass "red $proof proof enters RECOVERY without fault flag" || fail "red $proof proof enters RECOVERY without fault flag"
done
printf '%s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
