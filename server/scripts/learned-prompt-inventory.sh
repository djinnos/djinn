#!/usr/bin/env bash
#
# learned-prompt-inventory.sh — safe-by-default active-row export + evidence
# helper for the learned-prompt harvest (server/docs/learned-prompt-harvest.md).
#
# Runs the canonical inventory query in
#   server/scripts/learned-prompt-inventory.sql
# against a target database, writes a TSV export with a header row, and prints
# the row count + SHA-256 checksum + export path that an operator pastes into
# §4 of the harvest artifact.
#
# SAFETY
#   * Requires an explicit database URL via --db-url or DATABASE_URL/PG*/psql
#     environment. It refuses to run if no connection target is configured.
#   * It prints every command it runs (dry-run by default; pass --run to
#     execute). It NEVER embeds credentials, environment names, or secret
#     values in its output — only the connection *kind* (env var vs flag).
#   * It executes the query READ-ONLY. It does not write to the database.
#   * A worker environment must NOT point this at production/staging to
#     "fill in" the artifact. See §1.1 / §7 of the harvest artifact. The
#     helper is tooling; populating evidence fields is an operator action.
#
# USAGE
#   server/scripts/learned-prompt-inventory.sh [OPTIONS]
#
# OPTIONS
#   --db-url URL     Target PostgreSQL URL (libpq form). Preferred over env.
#   --out PATH       Export path (default: ./learned-prompt-inventory.tsv).
#   --run            Actually execute the query (default is --dry-run, which
#                    prints the plan without connecting).
#   --psql PATH      Path to the psql binary (default: psql from PATH).
#   -h, --help       Show this help.
#
# EXAMPLES
#   # Dry-run (no connection) — inspect what would run:
#   ./server/scripts/learned-prompt-inventory.sh --db-url "$DATABASE_URL"
#
#   # Execute and write evidence:
#   ./server/scripts/learned-prompt-inventory.sh --db-url "$DATABASE_URL" --run
#
#   # Use a PG* environment (e.g. PGHOST/PGUSER/PGDATABASE) instead of a URL:
#   ./server/scripts/learned-prompt-inventory.sh --run
#
# EVIDENCE OUTPUT
#   After a successful --run, the script prints a block suitable for §4:
#
#     --- learned-prompt inventory evidence ---
#     export_path:        <absolute path>
#     row_count:          <integer>
#     sha256:             <lowercase hex>
#     byte_size:          <integer>
#     query_file:         server/scripts/learned-prompt-inventory.sql
#     tool:               learned-prompt-inventory.sh
#     --- end evidence ---
#
# EXIT CODES
#   0 success (dry-run plan, or completed export)
#   2 usage error / missing connection target
#   3 psql/openssl/awk missing or query execution failed

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SQL_FILE="${SCRIPT_DIR}/learned-prompt-inventory.sql"
OUT_PATH="$(pwd)/learned-prompt-inventory.tsv"
PSQL_BIN="${PSQL_BIN:-psql}"
RUN=0
DB_URL=""

usage() {
    sed -n '2,/^EVIDENCE OUTPUT/p' "${BASH_SOURCE[0]}" \
        | sed 's/^# \{0,1\}//' >&2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --db-url)
            DB_URL="${2:?--db-url requires a value}"; shift 2 ;;
        --out)
            OUT_PATH="${2:?--out requires a value}"; shift 2 ;;
        --run)
            RUN=1; shift ;;
        --psql)
            PSQL_BIN="${2:?--psql requires a value}"; shift 2 ;;
        -h|--help)
            usage; exit 0 ;;
        *)
            echo "unknown option: $1" >&2; usage; exit 2 ;;
    esac
done

# --- resolve connection target -------------------------------------------------
# Accept an explicit --db-url, then the standard libpq DATABASE_URL, then a
# PG* environment (PGHOST/PGUSER/PGDATABASE...). We never print the URL/value
# itself; only which kind of target is in effect so an operator can confirm
# they are pointing at the intended environment.
conn_kind="none"
if [[ -n "$DB_URL" ]]; then
    conn_kind="db-url flag"
elif [[ -n "${DATABASE_URL:-}" ]]; then
    conn_kind="DATABASE_URL env"
elif [[ -n "${PGHOST:-}${PGDATABASE:-}" ]]; then
    conn_kind="PG* env"
fi

# --- precondition checks -------------------------------------------------------
need_cmd() {
    command -v "$1" >/dev/null 2>&1 || { echo "missing required command: $1" >&2; exit 3; }
}

if [[ ! -f "$SQL_FILE" ]]; then
    echo "inventory SQL not found: $SQL_FILE" >&2
    exit 3
fi

# Always validate tooling presence so dry-run output is trustworthy.
need_cmd "$PSQL_BIN"
need_cmd awk
need_cmd sha256sum

# --- compose the psql invocation ----------------------------------------------
# -X       never read ~/.psqlrc (deterministic output)
# -q       quiet: suppress BEGIN/COMMIT command-status tags so only the SELECT
#          rows reach stdout (keeps the TSV export and row count clean)
# -A       unaligned output
# -t       tuples only (no footer); we add the header manually below
# -F'\t'   tab-separated, matching the six-column TSV spec in the artifact
# -v ON_ERROR_STOP=1  abort on the first error
# -c/-f  wrap the query in a single READ ONLY transaction so a stray write
#        in the SQL file (there is none today) would be rejected.
psql_args=(-X -q -A -t -F$'\t' -v ON_ERROR_STOP=1
           -c 'BEGIN READ ONLY;' -f "$SQL_FILE" -c 'COMMIT;')

psql_conn=()
if [[ -n "$DB_URL" ]]; then
    psql_conn=("$DB_URL")
elif [[ -n "${DATABASE_URL:-}" ]]; then
    psql_conn=("$DATABASE_URL")
    # else: rely on PG* environment / service file; no positional URL.
fi

printf 'connection target: %s\n' "$conn_kind" >&2
printf 'sql file:          %s\n' "$SQL_FILE" >&2
printf 'export path:       %s\n' "$OUT_PATH" >&2
printf 'psql binary:       %s\n' "$PSQL_BIN" >&2

if [[ "$conn_kind" == "none" ]]; then
    cat >&2 <<'EOF'
ERROR: no database connection target configured.
Pass --db-url URL, set DATABASE_URL, or export PGHOST/PGUSER/PGDATABASE.
This helper intentionally refuses to guess a target environment.
EOF
    exit 2
fi

if [[ "$RUN" -eq 0 ]]; then
    cat >&2 <<EOF
DRY-RUN (no connection made). The following would be executed (read-only):

    "$PSQL_BIN" <conn> -X -q -A -t -F'\\t' -v ON_ERROR_STOP=1 \\
        -c 'BEGIN READ ONLY;' -f "$SQL_FILE" -c 'COMMIT;'
        | { print header; cat; } > "$OUT_PATH"

Re-run with --run to execute against the target environment (read-only).
EOF
    exit 0
fi

# --- execute (read-only) -------------------------------------------------------
# Build the export with a deterministic header row matching the §3.2 column
# order, then append the query output. A read-only transaction makes the
# intent explicit even though the query contains no writes.
{
    printf 'project_id\tagent_id\tagent_name\taction\tcreated_at\tamendment\n'
    "$PSQL_BIN" "${psql_conn[@]}" "${psql_args[@]}"
    # ON_ERROR_STOP=1 aborts on the first failing statement.
} > "$OUT_PATH"

# --- evidence -------------------------------------------------------------------
ROW_COUNT="$(awk 'NR > 1' "$OUT_PATH" | wc -l | tr -d ' ')"
SHA256="$(sha256sum "$OUT_PATH" | awk '{print $1}')"
BYTE_SIZE="$(wc -c < "$OUT_PATH" | tr -d ' ')"
ABS_OUT="$(cd "$(dirname "$OUT_PATH")" && pwd)/$(basename "$OUT_PATH")"

cat <<EOF
--- learned-prompt inventory evidence ---
export_path:        $ABS_OUT
row_count:          $ROW_COUNT
sha256:             $SHA256
byte_size:          $BYTE_SIZE
query_file:         server/scripts/learned-prompt-inventory.sql
tool:               learned-prompt-inventory.sh
--- end evidence ---
EOF
