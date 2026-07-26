#!/usr/bin/env bash
# Compile a Rust fixture while retaining /proc task-count samples for mold.
set -euo pipefail

THREADS=""
EVIDENCE_DIR=""
while (($#)); do
    case "$1" in
        --threads) THREADS="$2"; shift 2 ;;
        --evidence-dir) EVIDENCE_DIR="$2"; shift 2 ;;
        *) echo "usage: $0 --threads POSITIVE_N --evidence-dir DIR" >&2; exit 2 ;;
    esac
done
[[ "$THREADS" =~ ^[1-9][0-9]*$ ]] || { echo '--threads must be a positive integer' >&2; exit 2; }
[[ -n "$EVIDENCE_DIR" ]] || { echo '--evidence-dir is required' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
SAMPLES="$EVIDENCE_DIR/mold-task-counts.txt"
BUILD_LOG="$EVIDENCE_DIR/rust-build.log"
: > "$SAMPLES"
# Tests may provide a synthetic proc tree to deterministically prove process
# identification and fail-closed accounting. Production always uses /proc.
PROC_ROOT="${MOLD_SMOKE_PROC_ROOT:-/proc}"

# Every failure below prints a distinct, greppable reason plus the captured
# build log. The step used to abort with no output at all, which made a broken
# fixture indistinguishable from a cap violation and cost a release cycle.
fail() {
    echo "mold-thread-smoke: FAILED: $1" >&2
    shift
    if [[ -s "$BUILD_LOG" ]]; then
        echo '--- begin rust-build.log ---' >&2
        cat "$BUILD_LOG" >&2
        echo '--- end rust-build.log ---' >&2
    else
        echo '(rust-build.log is empty)' >&2
    fi
    exit "${1:-1}"
}

# Resolved before anything is built so a missing linker fails fast and loudly
# rather than as an unexplained "no mold observed" after a full compile.
MOLD_BIN="$(command -v mold 2>/dev/null || true)"
[[ -n "$MOLD_BIN" && -e "$MOLD_BIN" ]] \
    || fail 'no mold executable on PATH, so the thread cap cannot be observed' 2

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/src"
cat > "$work/Cargo.toml" <<'EOF'
[package]
name = "mold-thread-smoke"
version = "0.1.0"
edition = "2021"
EOF
# The fixture has to keep mold alive long enough to sample its worker pool. What
# does that is symbol and relocation count, not payload bytes: the previous
# 1048576-element zero array spent ~2s of rustc time and ~600MiB of RSS to
# produce a static that was optimized away entirely (measured: it added nothing
# to .data/.bss, so mold never saw it) and a link mold finished in ~5ms. Many
# small `#[used]` statics give mold real work for a fraction of the compile cost:
# 2000 of them build in ~0.2s and hold mold observable for ~10-20ms, which is
# where the 4-thread pool is actually visible.
#
# Only the statics are generated. Rust's `{}` placeholder stays inside a quoted
# heredoc so no printf format string can ever consume it again -- `%d` here was
# once substituted by printf itself, emitting `println!("0", ...)`, which fails
# to compile and took the whole release gate down.
{
    awk 'BEGIN {
        for (i = 0; i < 2000; i++) {
            printf "#[used]\npub static SMOKE_%d: [u64; 8] = [%d; 8];\n", i, i + 1
        }
    }'
    cat <<'RUST'
fn main() {
    println!("{}", SMOKE_0.len());
}
RUST
} > "$work/src/main.rs"

(
    cd "$work"
    RUSTC_WRAPPER='' cargo rustc --release -- \
        -C link-arg=-fuse-ld=mold -C "link-arg=-Wl,--threads=${THREADS}"
) > "$BUILD_LOG" 2>&1 &
build_pid=$!

# /proc is authoritative and works without procps in the runtime image.
#
# mold runs as two processes -- a parent that exits as soon as the output is
# written, plus the child that owns the `--threads` worker pool -- and the child
# is only alive for ~10-20ms. The pool is what this gate exists to measure, so
# the sweep has to be much faster than that window. It therefore does no forking
# at all: every earlier form (`readlink` per pid, `find` for the task count,
# `date` per sample, `sleep` per sweep) cost 1-2ms each, which made a sweep on a
# busy host slower than mold's entire lifetime. Measured: the forking version
# observed nothing at all on a 400-process host and failed successful builds.
#
# `-ef` compares device and inode through the /proc/<pid>/exe symlink, so it
# identifies the linker by the very file `mold` resolves to. That is strictly
# stronger than matching the basename, which a process merely named "mold" could
# satisfy. cmdline is unusable here: it names the invoking shell and this script.
exec 9>>"$SAMPLES"
while kill -0 "$build_pid" 2>/dev/null; do
    for proc in "$PROC_ROOT"/[0-9]*; do
        [[ "$proc/exe" -ef "$MOLD_BIN" ]] || continue
        # Count with a glob rather than `find`: a pid that exits mid-sweep makes
        # `find` exit non-zero, and under `set -e` that assignment aborted the
        # whole script silently with status 1 -- the exact signature that cost a
        # release cycle. Never record a zero: an empty sample would make $SAMPLES
        # non-empty and let a max of 0 satisfy the cap check, turning the
        # fail-closed guard into a fail-open one.
        count=0
        for task in "$proc"/task/*; do
            [[ -d "$task" ]] && count=$((count + 1))
        done
        (( count > 0 )) || continue
        printf '%s pid=%s tasks=%s\n' "${EPOCHREALTIME:-0}" "${proc#"$PROC_ROOT"/}" "$count" >&9
    done
done
exec 9>&-

# `wait` reports the build's status. Letting `set -e` act on it here aborted the
# script before any guard below could explain what happened, so capture it.
build_status=0
wait "$build_pid" || build_status=$?
(( build_status == 0 )) || fail "cargo build exited $build_status" "$build_status"

[[ -s "$SAMPLES" ]] || fail 'no mold process was observed during a successful build'
max="$(sed -nE 's/.*tasks=([0-9]+).*/\1/p' "$SAMPLES" | sort -n | tail -n1)"
# Unparseable evidence is treated as a violation, not as a pass.
if ! [[ "$max" =~ ^[0-9]+$ ]] || (( max > THREADS )); then
    fail "mold used $max tasks, exceeding configured thread cap $THREADS"
fi
printf 'configured_threads=%s maximum_observed_tasks=%s\n' "$THREADS" "$max" \
    | tee "$EVIDENCE_DIR/mold-task-count-summary.txt"
