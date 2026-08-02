#!/bin/sh
# Hermetic, repository-only tests for the PREP range and action interlocks.
# Every assertion judges the public shell entry point by its real exit code.
set -eu
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
GATE="$SCRIPT_DIR/check-cgroup-retirement-gate.sh"
FIXTURES="$SCRIPT_DIR/fixtures/cgroup-retirement/gate"
SCRATCH=$(mktemp -d /var/tmp/cgroup-retirement-gate.XXXXXX)
trap 'rm -rf "$SCRATCH"' EXIT INT TERM
PASS=0 FAIL=0
pass() { PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"; }
fail() { FAIL=$((FAIL + 1)); printf '  FAIL %s\n' "$1" >&2; }
expect_ok() {
    label=$1; shift
    if "$@" >/dev/null 2>&1; then pass "$label"; else fail "$label"; fi
}
expect_reject() {
    label=$1; shift
    set +e
    "$@" >/dev/null 2>&1
    code=$?
    set -e
    if [ "$code" -eq 1 ]; then pass "$label"; else fail "$label (exit $code)"; fi
}
new_repo() {
    dir=$1
    mkdir -p "$dir"
    git -C "$dir" init -q
    git -C "$dir" config user.email gate@example.invalid
    git -C "$dir" config user.name gate
    mkdir -p "$dir/server/crates/djinn-k8s/src"
    printf 'launcher remains armed\n' > "$dir/server/crates/djinn-k8s/src/launcher.rs"
    git -C "$dir" add . && git -C "$dir" commit -qm base
}
commit_file() {
    dir=$1 path=$2 content=$3
    mkdir -p "$dir/$(dirname "$path")"
    printf '%s\n' "$content" > "$dir/$path"
    git -C "$dir" add -A && git -C "$dir" commit -qm change
}
prep_case() {
    name=$1 path=$2 content=$3
    repo="$SCRATCH/$name"; new_repo "$repo"
    base=$(git -C "$repo" rev-parse HEAD)
    commit_file "$repo" "$path" "$content"
    head=$(git -C "$repo" rev-parse HEAD)
    printf '%s\n' "$repo $base $head"
}
prep_foundation_case() {
    repo="$SCRATCH/prep-foundation"; new_repo "$repo"
    base=$(git -C "$repo" rev-parse HEAD)
    # These are the production paths in the landed launcher-free environment
    # policy, rather than a lookalike launcher env.rs fixture.
    commit_file "$repo" server/crates/djinn-agent/src/environment.rs 'strict environment policy'
    commit_file "$repo" server/crates/djinn-agent/src/extension/handlers/workspace.rs 'clear admitted environment before spawning'
    commit_file "$repo" server/crates/djinn-agent/src/extension/tests/brokered_shell_program_tests.rs 'environment policy proof'
    commit_file "$repo" server/crates/djinn-agent/src/process_broker.rs 'broker forwards only admitted environment'
    commit_file "$repo" server/crates/djinn-agent/src/process.rs 'descendant reaping proof'
    commit_file "$repo" server/crates/djinn-sandbox/src/lib.rs 'sandbox proof'
    commit_file "$repo" scripts/fixtures/cgroup-retirement/schema.json '{"schema":"evidence"}'
    commit_file "$repo" scripts/check-cgroup-retirement-gate.sh 'guard work'
    printf '%s\n' "$repo $base $(git -C "$repo" rev-parse HEAD)"
}
sandbox_credential_proof_case() {
    repo="$SCRATCH/sandbox-credential-proof"; new_repo "$repo"
    # A minimal source-shaped copy of the immutable proof contract. The public
    # gate inspects HEAD, so this exercises deletion from its real linux.rs path.
    commit_file "$repo" server/crates/djinn-sandbox/src/linux.rs '#[test]
fn shell_sandbox_denies_reading_confidential_mount_contents() {
    SPEC_CANARY; CREDENTIAL_CANARY; TOKEN_CANARY;
    apply_with_confidential_roots();
    assert!(!direct.status.success());
    cargo build;
    assert!(!captured.contains(canary));
}'
    base=$(git -C "$repo" rev-parse HEAD)
    commit_file "$repo" server/crates/djinn-sandbox/src/linux.rs '#[test]
fn unrelated_sandbox_test() {}'
    printf '%s\n' "$repo $base $(git -C "$repo" rev-parse HEAD)"
}
sandbox_credential_proof_disablement_case() {
    repo="$SCRATCH/sandbox-credential-proof-disabled"; new_repo "$repo"
    # Keep every proof marker, but compile the test out. This must not be
    # accepted merely because the source-shaped proof text remains present.
    commit_file "$repo" server/crates/djinn-sandbox/src/linux.rs '#[test]
fn shell_sandbox_denies_reading_confidential_mount_contents() {
    SPEC_CANARY; CREDENTIAL_CANARY; TOKEN_CANARY;
    apply_with_confidential_roots();
    assert!(!direct.status.success());
    cargo build;
    assert!(!captured.contains(canary));
}'
    base=$(git -C "$repo" rev-parse HEAD)
    commit_file "$repo" server/crates/djinn-sandbox/src/linux.rs '# [ cfg(any()) ]
#[test]
fn shell_sandbox_denies_reading_confidential_mount_contents() {
    SPEC_CANARY; CREDENTIAL_CANARY; TOKEN_CANARY;
    apply_with_confidential_roots();
    assert!(!direct.status.success());
    cargo build;
    assert!(!captured.contains(canary));
}'
    printf '%s\n' "$repo $base $(git -C "$repo" rev-parse HEAD)"
}
confidential_roots_proof_cfg_attr_disablement_case() {
    repo="$SCRATCH/confidential-roots-proof-cfg-attr-disabled"; new_repo "$repo"
    commit_file "$repo" server/crates/djinn-sandbox/src/confidential.rs 'pub const CONFIDENTIAL_ROOTS: &[&str] = &["/var/run/djinn", "/var/run/secrets"];
#[test]
fn confidential_roots_cover_the_pod_secret_mounts() {}'
    base=$(git -C "$repo" rev-parse HEAD)
    # Rust permits whitespace between attribute tokens; preserve every mandatory
    # marker while disabling the other proof through cfg_attr.
    commit_file "$repo" server/crates/djinn-sandbox/src/confidential.rs 'pub const CONFIDENTIAL_ROOTS: &[&str] = &["/var/run/djinn", "/var/run/secrets"];
# [ cfg_attr(any(), ignore) ]
#[test]
fn confidential_roots_cover_the_pod_secret_mounts() {}'
    printf '%s\n' "$repo $base $(git -C "$repo" rev-parse HEAD)"
}
confidential_roots_proof_disablement_case() {
    repo="$SCRATCH/confidential-roots-proof-disabled"; new_repo "$repo"
    # Mutate the real confidential-roots proof path and preserve every marker:
    # marker-only validation must not let an ignored mandatory test pass.
    commit_file "$repo" server/crates/djinn-sandbox/src/confidential.rs 'pub const CONFIDENTIAL_ROOTS: &[&str] = &["/var/run/djinn", "/var/run/secrets"];
#[test]
fn confidential_roots_cover_the_pod_secret_mounts() {}'
    base=$(git -C "$repo" rev-parse HEAD)
    commit_file "$repo" server/crates/djinn-sandbox/src/confidential.rs 'pub const CONFIDENTIAL_ROOTS: &[&str] = &["/var/run/djinn", "/var/run/secrets"];
#[ignore]
#[test]
fn confidential_roots_cover_the_pod_secret_mounts() {}'
    printf '%s\n' "$repo $base $(git -C "$repo" rev-parse HEAD)"
}
real_proof_attribute_disablement_case() {
    name=$1 path=$2 proof_name=$3 attribute=$4 ordinary_count=${5:-0}
    repo="$SCRATCH/$name"; new_repo "$repo"
    mkdir -p "$repo/$(dirname "$path")"
    # Copy the production proof rather than synthesizing marker-shaped source.
    # The mutation preserves every mandatory marker while changing only the
    # attached outer-attribute block on the copied production proof.
    cp "$REPO_ROOT/$path" "$repo/$path"
    git -C "$repo" add "$path" && git -C "$repo" commit -qm proof-base
    base=$(git -C "$repo" rev-parse HEAD)
    node - "$repo/$path" "$proof_name" "$attribute" "$ordinary_count" <<'NODE'
const fs = require('fs');
const [path, proofName, disablingAttribute, ordinaryCount] = process.argv.slice(2);
const source = fs.readFileSync(path, 'utf8');
const needle = `    #[test]\n    fn ${proofName}`;
const ordinary = '    #[allow(dead_code)]\n'.repeat(Number(ordinaryCount));
const replacement = `${disablingAttribute}\n${ordinary}    #[test]\n    fn ${proofName}`;
if (!source.includes(needle)) throw new Error(`mandatory proof not found: ${proofName}`);
fs.writeFileSync(path, source.replace(needle, replacement));
NODE
    git -C "$repo" add "$path" && git -C "$repo" commit -qm disable-proof
    printf '%s\n' "$repo $base $(git -C "$repo" rev-parse HEAD)"
}
real_proof_source_mutation_case() {
    name=$1 path=$2 proof_name=$3 mutation=$4
    repo="$SCRATCH/$name"; new_repo "$repo"
    mkdir -p "$repo/$(dirname "$path")"
    cp "$REPO_ROOT/$path" "$repo/$path"
    git -C "$repo" add "$path" && git -C "$repo" commit -qm proof-base
    base=$(git -C "$repo" rev-parse HEAD)
    node - "$repo/$path" "$proof_name" "$mutation" <<'NODE'
const fs = require('fs');
const [path, proofName, mutation] = process.argv.slice(2);
const source = fs.readFileSync(path, 'utf8');
const needle = `    #[test]\n    fn ${proofName}`;
if (!source.includes(needle)) throw new Error(`mandatory proof not found: ${proofName}`);
const replacement = mutation === 'comment-bypass'
    ? `    #[test]\n    // fn ${proofName}()\n    #[cfg(any())]\n    fn ${proofName}`
    : mutation === 'macro-hash'
        ? `macro_rules! swallow_tokens { ($($tokens:tt)*) => {}; }\nswallow_tokens!(# an_unrelated_hash_token);\n${needle}`
        : `const UNRELATED_ATTRIBUTE_TEXT: &str = r"\n#[broken(\n";\n\n${needle}`;
fs.writeFileSync(path, source.replace(needle, replacement));
NODE
    git -C "$repo" add "$path" && git -C "$repo" commit -qm mutate-proof-source
    printf '%s\n' "$repo $base $(git -C "$repo" rev-parse HEAD)"
}
protected_deletion_case() {
    name=$1 path=$2
    repo="$SCRATCH/$name"; new_repo "$repo"
    if [ "$path" != 'server/crates/djinn-k8s/src/launcher.rs' ]; then
        commit_file "$repo" "$path" 'protected asset remains armed'
    fi
    base=$(git -C "$repo" rev-parse HEAD)
    git -C "$repo" rm -q "$path"
    git -C "$repo" commit -qm "delete-$name"
    # This deletion fixture is deliberately stronger than an allowlist test:
    # protectedPath must still win if a later change broadens PREP allow paths.
    expect_reject "PREP $name deletion is protected" env CGROUP_RETIREMENT_GATE_ROOT="$repo" "$GATE" --prep "$base" "$(git -C "$repo" rev-parse HEAD)"
}

printf 'Testing cgroup-retirement PREP range and fail-closed action gates\n'
set -- $(prep_foundation_case)
expect_ok 'PREP environment/reaping/sandbox/schema/guard range passes' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(sandbox_credential_proof_case)
expect_reject 'PREP rejects inline sandbox credential-proof deletion' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(sandbox_credential_proof_disablement_case)
expect_reject 'PREP rejects whitespace-bearing cfg-disabled sandbox credential proof' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(confidential_roots_proof_cfg_attr_disablement_case)
expect_reject 'PREP rejects whitespace-bearing cfg_attr-disabled confidential-roots proof' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(confidential_roots_proof_disablement_case)
expect_reject 'PREP rejects ignored confidential-roots credential proof' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(real_proof_attribute_disablement_case canonical-linux-proof server/crates/djinn-sandbox/src/linux.rs shell_sandbox_denies_reading_confidential_mount_contents '    #[cfg(any())]')
expect_reject 'PREP rejects canonical cfg-disabled real sandbox proof' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(real_proof_attribute_disablement_case malformed-linux-proof server/crates/djinn-sandbox/src/linux.rs shell_sandbox_denies_reading_confidential_mount_contents '    #[cfg(any()')
expect_reject 'PREP rejects malformed attribute before real sandbox proof' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(real_proof_attribute_disablement_case malformed-brace-linux-proof server/crates/djinn-sandbox/src/linux.rs shell_sandbox_denies_reading_confidential_mount_contents '    #[cfg(}')
expect_reject 'PREP rejects unmatched brace inside attached attribute on real sandbox proof' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(real_proof_attribute_disablement_case malformed-brace-confidential-proof server/crates/djinn-sandbox/src/confidential.rs confidential_roots_cover_the_pod_secret_mounts '    #[cfg(}')
expect_reject 'PREP rejects unmatched brace inside attached attribute on real confidential proof' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(real_proof_source_mutation_case commented-function-linux-proof server/crates/djinn-sandbox/src/linux.rs shell_sandbox_denies_reading_confidential_mount_contents comment-bypass)
expect_reject 'PREP ignores commented function text and rejects real cfg-disabled proof' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(real_proof_source_mutation_case unrelated-raw-attribute-text-linux-proof server/crates/djinn-sandbox/src/linux.rs shell_sandbox_denies_reading_confidential_mount_contents unrelated-raw-text)
expect_ok 'PREP ignores malformed attribute text in an unrelated raw string' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(real_proof_source_mutation_case nested-macro-hash-linux-proof server/crates/djinn-sandbox/src/linux.rs shell_sandbox_denies_reading_confidential_mount_contents macro-hash)
expect_ok 'PREP ignores an unrelated hash token in a completed macro invocation' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(real_proof_attribute_disablement_case multiline-cfg-attr-linux-proof server/crates/djinn-sandbox/src/linux.rs shell_sandbox_denies_reading_confidential_mount_contents '    #
    [
        cfg_attr(
            any(),
            ignore
        )
    ]')
expect_reject 'PREP rejects multiline cfg_attr-disabled real sandbox proof' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(real_proof_attribute_disablement_case separated-linux-proof server/crates/djinn-sandbox/src/linux.rs shell_sandbox_denies_reading_confidential_mount_contents '    #[cfg(any())]' 12)
expect_reject 'PREP rejects cfg-disabled real sandbox proof beyond ordinary attributes' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(real_proof_attribute_disablement_case multiline-cfg-confidential-proof server/crates/djinn-sandbox/src/confidential.rs confidential_roots_cover_the_pod_secret_mounts '    #
    [
        cfg(
            any()
        )
    ]')
expect_reject 'PREP rejects multiline cfg-disabled real confidential proof' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
set -- $(real_proof_attribute_disablement_case separated-confidential-proof server/crates/djinn-sandbox/src/confidential.rs confidential_roots_cover_the_pod_secret_mounts '    #[cfg(any())]' 12)
expect_reject 'PREP rejects cfg-disabled real confidential proof beyond ordinary attributes' env CGROUP_RETIREMENT_GATE_ROOT="$1" "$GATE" --prep "$2" "$3"
protected_deletion_case launcher server/crates/djinn-k8s/src/launcher.rs
protected_deletion_case render server/crates/djinn-k8s/src/job.rs
protected_deletion_case runtimeclass server/k8s/RuntimeClass.yaml
protected_deletion_case node server/node/cgroup-config.yaml
protected_deletion_case broker server/crates/djinn-cgroup-launcher/src/broker.rs
protected_deletion_case process-broker server/crates/djinn-agent/src/process_broker.rs
protected_deletion_case cgroup-kill server/crates/djinn-cgroup-launcher/src/bootstrap.rs
protected_deletion_case credential-boundary scripts/credential-boundary-test.sh

for status in failed refused skipped inconclusive stale; do
    input="$SCRATCH/$status.json"
    sed "s/\"sandbox_proof\": \"green\"/\"sandbox_proof\": \"$status\"/" "$FIXTURES/all-green.json" > "$input"
    expect_reject "deploy refuses $status mandatory proof" "$GATE" --deploy --candidate RETIRE_HEAD --inputs "$input"
done
expect_reject 'release refuses missing mandatory evidence input' "$GATE" --release --candidate RETIRE_HEAD --inputs "$FIXTURES/missing-evidence.json"
expect_reject 'node withdrawal refuses failed mandatory proof' "$GATE" --withdraw-node --candidate RETIRE_HEAD --inputs "$FIXTURES/failed-sandbox.json"

EVIDENCE_ROOT="$SCRATCH/evidence"
cp -R "$SCRIPT_DIR/fixtures/cgroup-retirement" "$EVIDENCE_ROOT"
node -e 'const fs=require("fs"),p=process.argv[1],x=JSON.parse(fs.readFileSync(p));x.identity_digests.evidence="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";fs.writeFileSync(p,JSON.stringify(x))' "$EVIDENCE_ROOT/candidates/RETIRE_HEAD.json"
expect_reject 'deploy refuses digest-mismatched evidence' env CGROUP_RETIREMENT_ROOT="$EVIDENCE_ROOT" "$GATE" --deploy --candidate RETIRE_HEAD --inputs "$FIXTURES/all-green.json"
expect_ok 'all-green repository evidence is candidate-review eligible only' "$GATE" --deploy --candidate RETIRE_HEAD --inputs "$FIXTURES/all-green.json"
printf '%s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
