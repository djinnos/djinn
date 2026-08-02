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

printf 'Testing immutable cgroup-retirement evidence fixtures\n'
if "$CHECKER" --candidate RETIRE_HEAD >/dev/null; then pass 'positive RETIRE_HEAD fixture'; else fail 'positive RETIRE_HEAD fixture'; fi
for fixture in "$FIXTURES"/*.json; do run_case "$(basename "$fixture" .json)"; done
printf '%s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
