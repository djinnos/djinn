#!/bin/sh
# Self-test harness for scripts/check-test-global-metrics.sh.
#
# Drives the production guard against a throwaway git repository this script
# creates (and always tears down) under a scratch directory. Pure POSIX shell
# plus git; no cargo, no network.
#
# A guard nobody has watched fail is not a guard. Every assertion expecting
# exit 1 exists to prove this one actually fires on the shapes it was written
# for. Every assertion expecting exit 0 exists to prove the exemptions are not
# so wide that the guard has stopped meaning anything — in particular that the
# two legitimate production readers (the /metrics HTTP handler and the health
# probe) stay out of scope even when they share a file with tests, and that an
# untouched pre-existing violation does not fail an unrelated PR.
#
# Run from anywhere:
#
#   sh scripts/test-check-test-global-metrics.sh
#
# Exits 0 on success, 1 if any assertion failed.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
GUARD="$SCRIPT_DIR/check-test-global-metrics.sh"

if [ ! -f "$GUARD" ]; then
    printf 'FATAL: production guard not found at %s\n' "$GUARD" >&2
    exit 2
fi

WORK=$(mktemp -d)
cleanup() { rm -rf -- "$WORK" 2>/dev/null || true; }
trap cleanup EXIT INT TERM

PASS=0
FAIL=0
pass() {
    PASS=$((PASS + 1))
    printf '  ok   %s\n' "$1"
}
fail() {
    FAIL=$((FAIL + 1))
    printf '  FAIL %s\n' "$1" >&2
    [ -n "${2:-}" ] && printf '       %s\n' "$2" >&2
    return 0
}

REPO="$WORK/repo"
mkdir -p "$REPO"
cd "$REPO"
git init -q .
git config user.email guard@example.com
git config user.name "Guard Self Test"
git config commit.gpgsign false

# ── base commit: a file with a PRE-EXISTING violation ─────────────────────
mkdir -p server/crates/legacy/src
cat >server/crates/legacy/src/lib.rs <<'EOF'
pub fn thing() -> u32 { 1 }

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_reads_the_global_registry() {
        let rendered = djinn_telemetry::render().unwrap();
        assert!(rendered.contains("x"));
    }
}
EOF
git add server/crates/legacy/src/lib.rs
git commit -qm base
BASE=$(git rev-parse HEAD)

expect() {
    label=$1
    want=$2
    shift 2
    set +e
    out=$("$GUARD" "$@" 2>&1)
    got=$?
    set -e
    if [ "$got" -eq "$want" ]; then
        pass "$label"
    else
        fail "$label" "expected exit $want, got $got: $(printf '%s' "$out" | tr '\n' ' ')"
    fi
}

echo "check-test-global-metrics self-test"

# 1. An untouched legacy violation must not fail an unrelated PR. This is the
#    load-bearing property of the added-lines scope: without it the guard is a
#    whole-tree migration order, and the file-size guard has already shown what
#    happens when compliance costs more than evasion.
mkdir -p server/crates/other/src
printf 'pub fn unrelated() {}\n' >server/crates/other/src/lib.rs
git add server/crates/other/src/lib.rs
git commit -qm "unrelated change"
expect "untouched legacy violation does not fail an unrelated change" 0 "$BASE"

# 2. Whole-tree mode still sees the legacy violation, so nothing is hidden.
expect "--all reports the legacy violation" 1 --all

# 3. A NEW read inside a #[cfg(test)] mod is a violation.
cat >>server/crates/other/src/lib.rs <<'EOF'

#[cfg(test)]
mod tests {
    #[test]
    fn new_read() {
        let rendered = djinn_telemetry::render().unwrap();
        assert!(rendered.is_empty());
    }
}
EOF
git add server/crates/other/src/lib.rs
git commit -qm "add a new global read"
expect "new read inside #[cfg(test)] mod is caught" 1 "$BASE"
git reset -q --hard HEAD~1

# 4. A NEW read in a *_tests.rs module a parent declares — the shape the
#    structural tracker alone misses, because the #[cfg(test)] lives in the
#    parent file — and from a bare helper fn with no #[test] attribute.
mkdir -p server/crates/other/src
cat >server/crates/other/src/helper_tests.rs <<'EOF'
fn snapshot() -> String {
    djinn_telemetry::render().expect("render")
}
EOF
git add server/crates/other/src/helper_tests.rs
git commit -qm "add helper tests module"
expect "new read in a *_tests.rs helper fn is caught" 1 "$BASE"
git reset -q --hard HEAD~1

# 5. Production readers are NOT violations, even in a file that also has tests
#    below them. If this ever flips, the guard has started ordering people to
#    break the /metrics endpoint.
mkdir -p server/src/server
cat >server/src/server/mod.rs <<'EOF'
pub async fn metrics_endpoint() -> String {
    match djinn_telemetry::render() {
        Ok(body) => body,
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn endpoint_exists() {
        assert!(true);
    }
}

pub fn health_probe() -> String {
    djinn_telemetry::render().unwrap_or_default()
}
EOF
git add server/src/server/mod.rs
git commit -qm "add production readers"
expect "production readers before and after a test mod are exempt" 0 "$BASE"
git reset -q --hard HEAD~1

# 6. The telemetry crate owns the singleton and must be able to test it.
mkdir -p server/crates/djinn-telemetry/src
cat >server/crates/djinn-telemetry/src/lib.rs <<'EOF'
#[cfg(test)]
mod tests {
    #[test]
    fn render_works() {
        let rendered = djinn_telemetry::render().unwrap();
        assert!(rendered.is_empty());
    }
}
EOF
git add server/crates/djinn-telemetry/src/lib.rs
git commit -qm "telemetry own tests"
expect "the djinn-telemetry crate is exempt" 0 "$BASE"
git reset -q --hard HEAD~1

# 7. A mention in a comment or a string is not a call.
cat >>server/crates/other/src/lib.rs <<'EOF'

#[cfg(test)]
mod comment_tests {
    #[test]
    fn mentions_only() {
        // djinn_telemetry::render() is what this test deliberately avoids.
        let s = "djinn_telemetry::render()";
        assert!(!s.is_empty());
    }
}
EOF
git add server/crates/other/src/lib.rs
git commit -qm "comment and string mentions"
expect "a comment or string mention is not a call" 0 "$BASE"
git reset -q --hard HEAD~1

# 8. The sanctioned replacements pass.
cat >>server/crates/other/src/lib.rs <<'EOF'

#[cfg(test)]
mod isolated_tests {
    #[test]
    fn sync_form() {
        let (_, rendered) = djinn_telemetry::render_isolated(|| emit());
        assert!(rendered.is_empty());
    }

    #[tokio::test]
    async fn async_form() {
        let recorder = djinn_telemetry::IsolatedRecorder::new();
        let _guard = recorder.scope();
        assert!(recorder.render().is_empty());
    }
}
EOF
git add server/crates/other/src/lib.rs
git commit -qm "isolated forms"
expect "render_isolated and IsolatedRecorder pass" 0 "$BASE"
git reset -q --hard HEAD~1

# 9. Usage errors are exit 2, distinct from a finding.
expect "missing argument is a usage error, not a finding" 2

printf '\n%s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
