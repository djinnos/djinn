#!/bin/sh
# Harvest contract validation for the learned-prompt runbook, inventory query,
# and prompt-equivalence helper.
#
# Validates the artifacts committed by epic t8p8 sibling tasks (mo9r, iykf,
# nywc) so that downstream removal work (epics 3x0w, 3sle, 8m3c) can rely on
# the harvest gate before proceeding.  This script does NOT touch any runtime,
# schema, API, MCP/REST, or UI code -- it is a read-only validation of
# repository-side documentation and helper scripts.
#
# Run from the repository root:
#
#   sh scripts/test-learned-prompt-harvest-contract.sh
#
# Exits 0 on success (all assertions pass).  The first failing assertion
# aborts with a non-zero status.
#
# WHAT IS VALIDATED
#
#   1. The harvest artifact (server/docs/learned-prompt-harvest.md) exists and
#      contains the required sections:
#        - Environment & timestamp (section 2)
#        - Row count, checksum & export reference (section 4)
#        - Disposition table (section 5)
#        - Prompt-equivalence evidence including byte-equivalence (section 6/6.1)
#        - Reviewer sign-off (section 7)
#
#   2. The active inventory query (server/scripts/learned-prompt-inventory.sql)
#      uses the exact runtime-active predicate:
#        - JOIN learned_prompt_history
#        - WHERE action IN ('keep','confirmed')
#        - ORDER BY a.project_id, a.id, lph.created_at ASC
#
#   3. The prompt-equivalence helper (server/scripts/learned-prompt-equivalence.sh)
#      demonstrates byte-identical comparison on its bundled fixtures:
#        - --selftest passes all three fixture regimes (byte-identity,
#          semantic-drift, removed)
#        - The byte-identity fixture pair is confirmed byte-identical
#
#   4. All fixture files required by the selftest are present.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

PASS=0
FAIL=0

pass() {
    PASS=$((PASS + 1))
    echo "  ok   $1"
}

fail() {
    FAIL=$((FAIL + 1))
    echo "  FAIL $1" >&2
}

assert_file() {
    if [ -f "$1" ]; then
        pass "file exists: $2"
    else
        fail "file missing: $2 ($1)"
    fi
}

assert_exec() {
    if [ -x "$1" ]; then
        pass "executable: $2"
    else
        fail "not executable: $2 ($1)"
    fi
}

assert_grep() {
    # $1 = fixed string, $2 = file, $3 = description
    if grep -qF -- "$1" "$2" 2>/dev/null; then
        pass "contains: $3"
    else
        fail "missing fragment: $3"
        echo "       expected: $1" >&2
        echo "       in file:  $2" >&2
    fi
}

assert_grep_re() {
    # $1 = regex pattern, $2 = file, $3 = description
    if grep -q -- "$1" "$2" 2>/dev/null; then
        pass "contains: $3"
    else
        fail "missing pattern: $3"
        echo "       expected: $1" >&2
        echo "       in file:  $2" >&2
    fi
}

cd "$REPO_ROOT"

echo "=== learned-prompt harvest contract validation ==="
echo

# --- 1. Harvest artifact exists and has required sections ---

echo "== 1. Harvest artifact sections =="

HARVEST="server/docs/learned-prompt-harvest.md"
assert_file "$HARVEST" "harvest artifact"

if [ -f "$HARVEST" ]; then
    assert_grep_re "## 2\. Environment"          "$HARVEST" "section 2: environment & timestamp"
    assert_grep_re "## 4\. Row count"            "$HARVEST" "section 4: row count / checksum / export"
    assert_grep_re "## 5\. Disposition"          "$HARVEST" "section 5: disposition table"
    assert_grep_re "## 6\. Prompt-equivalence"   "$HARVEST" "section 6: prompt-equivalence evidence"
    assert_grep_re "### 6\.1 Byte-equivalence"   "$HARVEST" "section 6.1: byte-equivalence"
    assert_grep_re "## 7\. Reviewer sign-off"    "$HARVEST" "section 7: reviewer sign-off"
fi

echo

# --- 2. Active inventory query contains required fragments ---

echo "== 2. Active inventory query fragments =="

SQL_FILE="server/scripts/learned-prompt-inventory.sql"
assert_file "$SQL_FILE" "inventory SQL"

if [ -f "$SQL_FILE" ]; then
    assert_grep "JOIN learned_prompt_history"                                "$SQL_FILE" "JOIN learned_prompt_history (SQL)"
    assert_grep "action IN ('keep','confirmed')"                            "$SQL_FILE" "active predicate (SQL)"
    assert_grep "ORDER BY a.project_id, a.id, lph.created_at ASC"          "$SQL_FILE" "deterministic ordering (SQL)"
fi

# The same fragments must appear in the harvest artifact (the runbook quotes
# the query verbatim in section 3).
if [ -f "$HARVEST" ]; then
    assert_grep "JOIN learned_prompt_history"                                "$HARVEST" "JOIN learned_prompt_history (runbook)"
    assert_grep "action IN ('keep','confirmed')"                            "$HARVEST" "active predicate (runbook)"
    assert_grep "ORDER BY a.project_id, a.id, lph.created_at ASC"          "$HARVEST" "deterministic ordering (runbook)"
fi

echo

# --- 3. Prompt-equivalence helper selftest ---

echo "== 3. Prompt-equivalence helper =="

EQUIV_SCRIPT="server/scripts/learned-prompt-equivalence.sh"
assert_file  "$EQUIV_SCRIPT" "equivalence helper"
assert_exec  "$EQUIV_SCRIPT" "equivalence helper"

# Verify all required fixture files are present.
FIXTURE_BASE="server/scripts/fixtures/learned-prompt-equivalence"
for regime in byte-identity semantic-drift removed; do
    for part in pre-assembled.prompt post-assembled.prompt; do
        assert_file "$FIXTURE_BASE/$regime/$part" "fixture $regime/$part"
    done
done

# Run the helper's built-in selftest, which exercises all three comparison
# modes (byte-identity, semantic, removed) against the bundled fixtures and
# verifies the byte-identity pair produces a PASS verdict.
if [ -x "$EQUIV_SCRIPT" ]; then
    SELFTEST_OUT=$("$EQUIV_SCRIPT" --selftest 2>&1) || {
        fail "selftest exited non-zero"
        echo "       output:" >&2
        echo "$SELFTEST_OUT" | sed 's/^/       /' >&2
    }

    # Parse selftest output for per-test results.
    for test_name in byte-identity semantic-drift removed; do
        if echo "$SELFTEST_OUT" | grep -q -- "PASS \[$test_name\]"; then
            pass "selftest: $test_name PASS"
        else
            fail "selftest: $test_name did not report PASS"
            echo "       output:" >&2
            echo "$SELFTEST_OUT" | sed 's/^/       /' >&2
        fi
    done

    # Verify the overall selftest reports 0 failures.
    if echo "$SELFTEST_OUT" | grep -q -- "0 failure(s)"; then
        pass "selftest: 0 total failures"
    else
        fail "selftest reported failures"
        echo "       output:" >&2
        echo "$SELFTEST_OUT" | sed 's/^/       /' >&2
    fi
fi

# Verify byte-identity fixtures are truly byte-identical (independent of the
# helper's own selftest -- a direct cmp).
BI_PRE="$FIXTURE_BASE/byte-identity/pre-assembled.prompt"
BI_POST="$FIXTURE_BASE/byte-identity/post-assembled.prompt"
if [ -f "$BI_PRE" ] && [ -f "$BI_POST" ]; then
    if cmp -s "$BI_PRE" "$BI_POST"; then
        pass "byte-identity fixtures are byte-identical (direct cmp)"
    else
        fail "byte-identity fixtures differ (direct cmp)"
    fi
fi

# Also verify that semantic-drift and removed fixtures are NOT byte-identical
# (as documented -- those pairs are intentionally different).
for regime in semantic-drift removed; do
    PRE="$FIXTURE_BASE/$regime/pre-assembled.prompt"
    POST="$FIXTURE_BASE/$regime/post-assembled.prompt"
    if [ -f "$PRE" ] && [ -f "$POST" ]; then
        if cmp -s "$PRE" "$POST"; then
            fail "$regime fixtures are unexpectedly byte-identical"
        else
            pass "$regime fixtures are not byte-identical (as expected)"
        fi
    fi
done

echo

# --- Summary ---

TOTAL=$((PASS + FAIL))
echo "=== harvest contract validation: $PASS/$TOTAL passed"
if [ "$FAIL" -gt 0 ]; then
    echo "  $FAIL FAILED"
    exit 1
fi
echo "=== all checks passed ==="
exit 0
