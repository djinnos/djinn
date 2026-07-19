#!/usr/bin/env bash
# Deterministic contract checks for image-CI helper input validation.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
for bad in 0 -1 nope; do
    if "$ROOT/scripts/image-ci/run-mold-thread-smoke.sh" --threads "$bad" --evidence-dir /dev/null >/dev/null 2>&1; then
        echo "accepted invalid thread count: $bad" >&2
        exit 1
    fi
done
# This fixture-free check ensures the compatibility parser rejects a missing
# installed mold rather than treating absent evidence as compatible.
if PATH=/nonexistent "$ROOT/scripts/image-ci/probe-mold-compatibility.sh" --evidence-dir "$(mktemp -d)" >/dev/null 2>&1; then
    echo 'compatibility probe accepted missing mold' >&2
    exit 1
fi

# The runtime invocation names this script in bash's cmdline. A synthetic proc
# tree makes sure that cannot be mistaken for mold and that no observation is
# still fail-closed. A second fixture proves task accounting is only recorded
# for an executable actually named mold.
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
fake_bin="$fixture/bin"
fake_proc="$fixture/proc"
mkdir -p "$fake_bin" "$fake_proc/101/task/worker"
cat > "$fake_bin/cargo" <<'EOF'
#!/usr/bin/env bash
sleep 0.2
EOF
chmod +x "$fake_bin/cargo"
ln -s /usr/bin/bash "$fake_proc/101/exe"

if PATH="$fake_bin:$PATH" MOLD_SMOKE_PROC_ROOT="$fake_proc" \
    "$ROOT/scripts/image-ci/run-mold-thread-smoke.sh" --threads 2 --evidence-dir "$fixture/no-mold" >/dev/null 2>&1; then
    echo 'thread smoke accepted a non-mold executable as linker evidence' >&2
    exit 1
fi

touch "$fake_bin/mold"
ln -sf "$fake_bin/mold" "$fake_proc/101/exe"
mkdir -p "$fake_proc/101/task/second-worker"
PATH="$fake_bin:$PATH" MOLD_SMOKE_PROC_ROOT="$fake_proc" \
    "$ROOT/scripts/image-ci/run-mold-thread-smoke.sh" --threads 2 --evidence-dir "$fixture/mold" >/dev/null
grep -Fxq 'configured_threads=2 maximum_observed_tasks=2' "$fixture/mold/mold-task-count-summary.txt"
printf 'ok: image-ci helper input contracts\n'
