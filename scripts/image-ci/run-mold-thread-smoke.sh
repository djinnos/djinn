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
: > "$SAMPLES"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/src"
cat > "$work/Cargo.toml" <<'EOF'
[package]
name = "mold-thread-smoke"
version = "0.1.0"
edition = "2021"
EOF
# A substantial static payload keeps the final link observable without relying
# on host CPU topology or any external source tree.
{
    printf 'pub static PAYLOAD: [u64; 1048576] = ['
    awk 'BEGIN { for (i = 0; i < 1048576; i++) printf "0," }'
    printf '];\nfn main() { println!("%d", PAYLOAD.len()); }\n'
} > "$work/src/main.rs"

(
    cd "$work"
    RUSTC_WRAPPER= cargo rustc --release -- \
        -C link-arg=-fuse-ld=mold -C "link-arg=-Wl,--threads=${THREADS}"
) > "$EVIDENCE_DIR/rust-build.log" 2>&1 &
build_pid=$!

# /proc is authoritative and works without procps in the runtime image.
while kill -0 "$build_pid" 2>/dev/null; do
    for proc in /proc/[0-9]*; do
        pid="${proc#/proc/}"
        [[ -r "$proc/cmdline" && -d "$proc/task" ]] || continue
        cmdline="$(tr '\0' ' ' < "$proc/cmdline" 2>/dev/null || true)"
        case "$cmdline" in
            *mold*)
                count="$(find "$proc/task" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)"
                printf '%s pid=%s tasks=%s\n' "$(date -u +%FT%T.%NZ)" "$pid" "$count" >> "$SAMPLES"
                ;;
        esac
    done
    sleep 0.002
done
wait "$build_pid"

[[ -s "$SAMPLES" ]] || { echo 'no mold process was observed; see rust-build.log' >&2; exit 1; }
max="$(sed -nE 's/.*tasks=([0-9]+).*/\1/p' "$SAMPLES" | sort -n | tail -n1)"
[[ "$max" =~ ^[0-9]+$ ]] && (( max <= THREADS )) || {
    echo "mold used $max tasks, exceeding configured thread cap $THREADS" >&2
    exit 1
}
printf 'configured_threads=%s maximum_observed_tasks=%s\n' "$THREADS" "$max" \
    | tee "$EVIDENCE_DIR/mold-task-count-summary.txt"
