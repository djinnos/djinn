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
# The sampler identifies the linker by device+inode of whatever `mold` resolves
# to on PATH, so the fixture supplies its own executable stand-in. That keeps
# these checks independent of whether a real mold is installed here.
printf '#!/usr/bin/env bash\n' > "$fake_bin/mold"
chmod +x "$fake_bin/mold"
ln -s /usr/bin/bash "$fake_proc/101/exe"
# Naming comm "mold" keeps the negative case honest: nothing about the name
# should be enough to admit a pid whose exe is bash.
printf 'mold\n' > "$fake_proc/101/comm"

if PATH="$fake_bin:$PATH" MOLD_SMOKE_PROC_ROOT="$fake_proc" \
    "$ROOT/scripts/image-ci/run-mold-thread-smoke.sh" --threads 2 --evidence-dir "$fixture/no-mold" >/dev/null 2>&1; then
    echo 'thread smoke accepted a non-mold executable as linker evidence' >&2
    exit 1
fi
# A build that never produced a mold process must say so, distinctly, and must
# surface the build log rather than exiting mute.
no_mold_output="$(PATH="$fake_bin:$PATH" MOLD_SMOKE_PROC_ROOT="$fake_proc" \
    "$ROOT/scripts/image-ci/run-mold-thread-smoke.sh" --threads 2 --evidence-dir "$fixture/no-mold-msg" 2>&1 || true)"
grep -Fq 'FAILED: no mold process was observed' <<<"$no_mold_output" || {
    echo 'no-mold failure did not report a distinct reason' >&2
    printf '%s\n' "$no_mold_output" >&2
    exit 1
}

ln -sf "$fake_bin/mold" "$fake_proc/101/exe"
mkdir -p "$fake_proc/101/task/second-worker"
PATH="$fake_bin:$PATH" MOLD_SMOKE_PROC_ROOT="$fake_proc" \
    "$ROOT/scripts/image-ci/run-mold-thread-smoke.sh" --threads 2 --evidence-dir "$fixture/mold" >/dev/null
grep -Fxq 'configured_threads=2 maximum_observed_tasks=2' "$fixture/mold/mold-task-count-summary.txt"

# Exceeding the cap must stay fail-closed and name the observed count. Three task
# directories against --threads 2 is a violation.
mkdir -p "$fake_proc/101/task/third-worker"
cap_output="$(PATH="$fake_bin:$PATH" MOLD_SMOKE_PROC_ROOT="$fake_proc" \
    "$ROOT/scripts/image-ci/run-mold-thread-smoke.sh" --threads 2 --evidence-dir "$fixture/cap" 2>&1 || true)"
grep -Fq 'FAILED: mold used 3 tasks, exceeding configured thread cap 2' <<<"$cap_output" || {
    echo 'cap violation did not report the observed task count' >&2
    printf '%s\n' "$cap_output" >&2
    exit 1
}
if PATH="$fake_bin:$PATH" MOLD_SMOKE_PROC_ROOT="$fake_proc" \
    "$ROOT/scripts/image-ci/run-mold-thread-smoke.sh" --threads 2 --evidence-dir "$fixture/cap2" >/dev/null 2>&1; then
    echo 'thread smoke passed while mold exceeded the configured cap' >&2
    exit 1
fi
rm -rf "$fake_proc/101/task/third-worker"

# A failing build must be reported as a build failure -- with the compiler output
# echoed to stderr -- and must never be silently conflated with a cap violation.
# `wait` returning non-zero under `set -e` used to abort the script with no
# output whatsoever, which is what made this gate so expensive to triage.
cat > "$fake_bin/cargo" <<'EOF'
#!/usr/bin/env bash
echo '   Compiling mold-thread-smoke v0.1.0'
echo 'error[E0425]: cannot find value `nope` in this scope' >&2
exit 101
EOF
chmod +x "$fake_bin/cargo"
build_output="$(PATH="$fake_bin:$PATH" MOLD_SMOKE_PROC_ROOT="$fake_proc" \
    "$ROOT/scripts/image-ci/run-mold-thread-smoke.sh" --threads 2 --evidence-dir "$fixture/badbuild" 2>&1 || true)"
grep -Fq 'FAILED: cargo build exited 101' <<<"$build_output" || {
    echo 'build failure did not report the build exit status' >&2
    printf '%s\n' "$build_output" >&2
    exit 1
}
grep -Fq 'error[E0425]: cannot find value' <<<"$build_output" || {
    echo 'build failure did not echo the captured build log' >&2
    printf '%s\n' "$build_output" >&2
    exit 1
}
if PATH="$fake_bin:$PATH" MOLD_SMOKE_PROC_ROOT="$fake_proc" \
    "$ROOT/scripts/image-ci/run-mold-thread-smoke.sh" --threads 2 --evidence-dir "$fixture/badbuild2" >/dev/null 2>&1; then
    echo 'thread smoke passed despite a failing build' >&2
    exit 1
fi
printf 'ok: image-ci helper input contracts\n'
