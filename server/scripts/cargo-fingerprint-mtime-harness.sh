#!/usr/bin/env bash
# Cargo 1.97 fingerprint-mtime validation harness.
#
# Reproduces the evidence from
# research/technical/cargo-1-97-fingerprint-mtimes-do-not-record-artifact-reuse
# using the fixture under server/scripts/fixtures/cargo-fingerprint-mtime.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_FIXTURE="${SCRIPT_DIR}/fixtures/cargo-fingerprint-mtime"
FIXTURE="${DEFAULT_FIXTURE}"
TARGET_DIR=""
TARGET_TRIPLE="x86_64-unknown-linux-gnu"
SLEEP_SECONDS=2

usage() {
    echo "Usage: $0 --target-dir <path> [--fixture <dir>] [--target-triple <triple>] [--sleep <seconds>]"
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target-dir)
            TARGET_DIR="${2:-}"
            shift 2
            ;;
        --fixture)
            FIXTURE="${2:-}"
            shift 2
            ;;
        --target-triple)
            TARGET_TRIPLE="${2:-}"
            shift 2
            ;;
        --sleep)
            SLEEP_SECONDS="${2:-}"
            shift 2
            ;;
        -h|--help)
            usage
            ;;
        *)
            echo "Unknown argument: $1" >&2
            usage
            ;;
    esac
done

if [[ -z "${TARGET_DIR}" ]]; then
    echo "ERROR: --target-dir is required" >&2
    usage
fi

if [[ ! -f "${FIXTURE}/Cargo.toml" ]]; then
    echo "ERROR: fixture missing Cargo.toml at ${FIXTURE}" >&2
    exit 1
fi

if [[ -e "${TARGET_DIR}" && "$(find "${TARGET_DIR}" -mindepth 1 -print -quit 2>/dev/null)" ]]; then
    echo "ERROR: target-dir must be empty or non-existent: ${TARGET_DIR}" >&2
    exit 1
fi

mkdir -p "${TARGET_DIR}"
TARGET_DIR="$(cd "${TARGET_DIR}" && pwd)"
FIXTURE="$(cd "${FIXTURE}" && pwd)"

export CARGO_INCREMENTAL=1
export RUSTC_WRAPPER=""
export CARGO_NET_OFFLINE=true
export CARGO_TARGET_DIR="${TARGET_DIR}"

cd "${FIXTURE}"

echo "== Environment =="
CARGO_VERSION="$(cargo --version)"
RUSTC_VERSION="$(rustc --version)"
echo "cargo: ${CARGO_VERSION}"
echo "rustc: ${RUSTC_VERSION}"
echo "installed targets:"
rustup target list --installed | sed 's/^/  /'

FS_TYPE="$( (findmnt -T "${TARGET_DIR}" -o FSTYPE -n 2>/dev/null) || (df -T "${TARGET_DIR}" | awk 'NR==2 {print $2}') )"
# Prefer stat -c for a known numeric value; fall back to stat -f only if the
# output looks like a positive integer, otherwise report null.
BLOCK_SIZE="$(stat -c '%o' "${TARGET_DIR}" 2>/dev/null)"
if [[ -z "${BLOCK_SIZE}" ]]; then
    BLOCK_SIZE="$(stat -f "${TARGET_DIR}" 2>/dev/null | awk '/Fundamental block size/{print $4}')"
fi
# Normalize block_size to a bare integer; some stat -f variants emit
# trailing descriptive text after the number.
BLOCK_SIZE="$(echo "${BLOCK_SIZE}" | awk '{print $1}')"
if [[ -z "${BLOCK_SIZE}" || "${BLOCK_SIZE}" =~ [^0-9] ]]; then
    BLOCK_SIZE="null"
fi
echo "target filesystem: ${FS_TYPE}"
echo "block size: ${BLOCK_SIZE}"

if ! rustup target list --installed | grep -qx "${TARGET_TRIPLE}"; then
    echo "SKIP: target triple ${TARGET_TRIPLE} is not installed" >&2
    exit 0
fi

snapshot_fingerprints() {
    local out="$1"
    find "${TARGET_DIR}" -type f -path '*/.fingerprint/*/*' -printf '%p|%T@\n' 2>/dev/null | sort > "${out}"
}

stage_mtimes_changed() {
    local before="$1"
    local after="$2"
    if diff -q "${before}" "${after}" >/dev/null; then
        echo 0
    else
        echo 1
    fi
}

json_bool() {
    if [[ "$1" == "1" ]]; then echo "true"; else echo "false"; fi
}

RESULT_STAGES=()

# Stage 1: initial debug build
STEP=1
echo ""
echo "== Stage ${STEP}: initial debug build =="
LOG="${TARGET_DIR}/stage-${STEP}.log"
cargo clippy --all -vv > "${LOG}" 2>&1
SNAP1="${TARGET_DIR}/snapshot-${STEP}.psv"
snapshot_fingerprints "${SNAP1}"
COUNT1="$(wc -l < "${SNAP1}")"
echo "fingerprint files: ${COUNT1}"

# Stage 2: Fresh/no-op debug build
STEP=2
echo ""
echo "== Stage ${STEP}: no-op debug build (sleep ${SLEEP_SECONDS}s) =="
sleep "${SLEEP_SECONDS}"
LOG="${TARGET_DIR}/stage-${STEP}.log"
cargo clippy --all -vv > "${LOG}" 2>&1
tail -5 "${LOG}"
SNAP2="${TARGET_DIR}/snapshot-${STEP}.psv"
snapshot_fingerprints "${SNAP2}"
CHANGED2="$(stage_mtimes_changed "${SNAP1}" "${SNAP2}")"
echo "fingerprint mtimes changed: ${CHANGED2}"

# Stage 3: app-only rebuild with dependency reuse
STEP=3
echo ""
echo "== Stage ${STEP}: app-only rebuild (sleep ${SLEEP_SECONDS}s) =="
sleep "${SLEEP_SECONDS}"
touch app/src/main.rs
LOG="${TARGET_DIR}/stage-${STEP}.log"
cargo clippy --all -vv > "${LOG}" 2>&1
tail -5 "${LOG}"
SNAP3="${TARGET_DIR}/snapshot-${STEP}.psv"
snapshot_fingerprints "${SNAP3}"
CHANGED3="$(stage_mtimes_changed "${SNAP2}" "${SNAP3}")"
echo "fingerprint mtimes changed: ${CHANGED3}"

# Stage 4: release initial build
STEP=4
echo ""
echo "== Stage ${STEP}: release build (sleep ${SLEEP_SECONDS}s) =="
sleep "${SLEEP_SECONDS}"
LOG="${TARGET_DIR}/stage-${STEP}.log"
cargo clippy --all --release -vv > "${LOG}" 2>&1
tail -5 "${LOG}"
SNAP4="${TARGET_DIR}/snapshot-${STEP}.psv"
snapshot_fingerprints "${SNAP4}"

# Stage 5: release no-op build
STEP=5
echo ""
echo "== Stage ${STEP}: release no-op build (sleep ${SLEEP_SECONDS}s) =="
sleep "${SLEEP_SECONDS}"
LOG="${TARGET_DIR}/stage-${STEP}.log"
cargo clippy --all --release -vv > "${LOG}" 2>&1
tail -5 "${LOG}"
SNAP5="${TARGET_DIR}/snapshot-${STEP}.psv"
snapshot_fingerprints "${SNAP5}"
CHANGED5="$(stage_mtimes_changed "${SNAP4}" "${SNAP5}")"
echo "fingerprint mtimes changed: ${CHANGED5}"

# Stage 6: explicit target triple initial build
STEP=6
echo ""
echo "== Stage ${STEP}: explicit target ${TARGET_TRIPLE} (sleep ${SLEEP_SECONDS}s) =="
sleep "${SLEEP_SECONDS}"
LOG="${TARGET_DIR}/stage-${STEP}.log"
cargo clippy --all --target "${TARGET_TRIPLE}" -vv > "${LOG}" 2>&1 || true
tail -5 "${LOG}"
SNAP6="${TARGET_DIR}/snapshot-${STEP}.psv"
snapshot_fingerprints "${SNAP6}"

# Stage 7: explicit target triple no-op build
STEP=7
if [[ -s "${SNAP6}" ]]; then
    echo ""
    echo "== Stage ${STEP}: explicit target no-op (sleep ${SLEEP_SECONDS}s) =="
    sleep "${SLEEP_SECONDS}"
    LOG="${TARGET_DIR}/stage-${STEP}.log"
    cargo clippy --all --target "${TARGET_TRIPLE}" -vv > "${LOG}" 2>&1 || true
    tail -5 "${LOG}"
    SNAP7="${TARGET_DIR}/snapshot-${STEP}.psv"
    snapshot_fingerprints "${SNAP7}"
    CHANGED7="$(stage_mtimes_changed "${SNAP6}" "${SNAP7}")"
    echo "fingerprint mtimes changed: ${CHANGED7}"
else
    CHANGED7="0"
    echo "Skipped explicit target no-op (no artifacts produced)."
fi

# Stage 8: mtime-preserving seeded-target reuse
STEP=8
echo ""
echo "== Stage ${STEP}: seeded target copy (sleep ${SLEEP_SECONDS}s) =="
SEED_TARGET="${TARGET_DIR}-seeded"
rm -rf "${SEED_TARGET}"
cp -a "${TARGET_DIR}" "${SEED_TARGET}"
export CARGO_TARGET_DIR="${SEED_TARGET}"
SNAP8A="${TARGET_DIR}/snapshot-seeded-before.psv"
find "${SEED_TARGET}" -type f -path '*/.fingerprint/*/*' -printf '%p|%T@\n' 2>/dev/null | sort > "${SNAP8A}"
sleep "${SLEEP_SECONDS}"
LOG="${TARGET_DIR}/stage-${STEP}.log"
cargo clippy --all -vv > "${LOG}" 2>&1
tail -5 "${LOG}"
SNAP8B="${TARGET_DIR}/snapshot-seeded-after.psv"
find "${SEED_TARGET}" -type f -path '*/.fingerprint/*/*' -printf '%p|%T@\n' 2>/dev/null | sort > "${SNAP8B}"
CHANGED8="$(stage_mtimes_changed "${SNAP8A}" "${SNAP8B}")"
echo "seeded fingerprint mtimes changed: ${CHANGED8}"

# Emit evidence.json
EVIDENCE="${TARGET_DIR}/evidence.json"
cat > "${EVIDENCE}" <<EOF
{
  "cargo_version": "${CARGO_VERSION}",
  "rustc_version": "${RUSTC_VERSION}",
  "target_triple": "${TARGET_TRIPLE}",
  "installed_targets": [$(rustup target list --installed | awk '{print "\"" $0 "\""}' | paste -sd ',')],
  "filesystem": {
    "type": "${FS_TYPE}",
    "block_size": ${BLOCK_SIZE}
  },
  "env": {
    "CARGO_INCREMENTAL": "${CARGO_INCREMENTAL}",
    "RUSTC_WRAPPER": "${RUSTC_WRAPPER}",
    "CARGO_NET_OFFLINE": "${CARGO_NET_OFFLINE}"
  },
  "stages": {
    "no_op_debug": { "fingerprint_mtimes_changed": $(json_bool "${CHANGED2}") },
    "app_only_rebuild": { "fingerprint_mtimes_changed": $(json_bool "${CHANGED3}") },
    "no_op_release": { "fingerprint_mtimes_changed": $(json_bool "${CHANGED5}") },
    "no_op_explicit_target": { "fingerprint_mtimes_changed": $(json_bool "${CHANGED7}") },
    "seeded_target_no_op": { "fingerprint_mtimes_changed": $(json_bool "${CHANGED8}") }
  }
}
EOF

echo ""
echo "== Evidence written to ${EVIDENCE} =="
cat "${EVIDENCE}"
