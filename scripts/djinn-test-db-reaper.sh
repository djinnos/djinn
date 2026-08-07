#!/usr/bin/env bash
#
# Reclaim `djinn_test_<uuidv7>` clone databases left behind by the DB test
# harness.
#
# Two callers, one predicate:
#
#   * CI (`--loop`): `.github/workflows/quality-gate.yml` starts this in the
#     background and sets `DJINN_TEST_DB_REAPER=external` for the test step, so
#     `TestDbInit::drop` skips its synchronous `DROP DATABASE` (see
#     `server/crates/djinn-db/src/database.rs`). The reaper is then the ONLY
#     thing bounding the runner's disk, which is why it must actually reap —
#     `--loop` prints a running total so a wedged reaper is visible in the job
#     log rather than silently filling the runner.
#
#   * Local (`make test-db-sweep`): one shot. Hard-killed runs (SIGKILL, nextest
#     `terminate-after`) skip `Drop` entirely and strand ~16 MB per test; this
#     is how you get that space back without `make test-db-reset`, which also
#     destroys and rebuilds the template.
#
# The safety predicate — both callers use it — is:
#
#   1. the name matches `djinn_test_<32 lowercase hex>` exactly, so
#      `djinn_test_template` and any human-named database can never match; AND
#   2. the UUIDv7 embedded in the name (its first 48 bits are a unix
#      millisecond timestamp) is older than --min-age-seconds; AND
#   3. no backend is currently connected to it.
#
# (2) and (3) are both required. (3) alone would race the window between
# `CREATE DATABASE … TEMPLATE` and the lazily-connected pool's first query.
# (2) alone would kill a legitimately long-running test. Together, a database
# has to have been created longer ago than any test may run AND have no live
# session before it is touched. `DROP DATABASE` is issued WITHOUT `WITH
# (FORCE)` on purpose: if a session appears between the query and the drop,
# Postgres refuses and the reaper moves on instead of destroying a live test's
# database.
set -euo pipefail

PSQL_URL="${DJINN_TEST_DATABASE_URL:-postgres://postgres:postgres@127.0.0.1:5433/postgres}"
# Longer than any single test can run: nextest's `slow-timeout` in
# `server/.config/nextest.toml` is `period = 30s, terminate-after = 3`, so a
# test process is killed at 90s. 150s leaves a margin over that hard ceiling.
MIN_AGE_SECONDS=150
INTERVAL_SECONDS=15
LOOP=0
# Hard ceiling on how many finished-but-unreclaimed clones may accumulate.
#
# Needed because --min-age-seconds bounds LATENCY, not VOLUME: peak outstanding
# is roughly (test threads / test duration) x (min-age + interval), so the
# faster the suite gets the more disk it holds. At ~16 MB per clone and 4-way
# concurrency, a 0.5s test would park ~1.2 GB/minute of min-age. A hosted
# runner that has just built ~12 GB of artifacts cannot absorb that, and its
# failure mode is the runner process dying on ENOSPC (see
# CI_RUNNER_DISK_PREFLIGHT in .github/workflows/quality-gate.yml), not a test
# failure anyone can read.
#
# Above the cap the age requirement drops to MIN_AGE_FLOOR_SECONDS. That stays
# safe because age is NOT what proves a test is finished — "no live session"
# is. The age guard exists only to cover the window between `CREATE DATABASE …
# TEMPLATE` returning and the lazily-connected pool's first query, and in
# `Database::ensure_initialized` those are adjacent statements (sub-10ms), so
# even the floor is a ~1000x margin. A test that is still running holds a
# pooled connection (sqlx idle_timeout defaults to 10 minutes, far beyond
# nextest's 90s hard kill) and is therefore invisible to every query here.
MAX_OUTSTANDING=0
MIN_AGE_FLOOR_SECONDS=10

usage() {
  cat >&2 <<'USAGE'
usage: djinn-test-db-reaper.sh [--loop] [--interval-seconds N] [--min-age-seconds N] [--url DSN]

  --loop                 sweep forever (CI); default is a single sweep
  --interval-seconds N   seconds between sweeps in --loop mode (default 15)
  --min-age-seconds N    only reap clones created more than N seconds ago (default 150)
  --max-outstanding N    hard cap on unreclaimed clones; above it the age requirement
                         drops to 10s so disk stays bounded (default 0 = no cap)
  --url DSN              admin DSN (default $DJINN_TEST_DATABASE_URL, else the :5433 dev server)
USAGE
  exit 2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --loop) LOOP=1; shift ;;
    --interval-seconds) INTERVAL_SECONDS="${2:?--interval-seconds needs a value}"; shift 2 ;;
    --min-age-seconds) MIN_AGE_SECONDS="${2:?--min-age-seconds needs a value}"; shift 2 ;;
    --max-outstanding) MAX_OUTSTANDING="${2:?--max-outstanding needs a value}"; shift 2 ;;
    --url) PSQL_URL="${2:?--url needs a value}"; shift 2 ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done

case "$MIN_AGE_SECONDS" in ''|*[!0-9]*) echo "--min-age-seconds must be a non-negative integer" >&2; exit 2 ;; esac
case "$INTERVAL_SECONDS" in ''|*[!0-9]*) echo "--interval-seconds must be a positive integer" >&2; exit 2 ;; esac
case "$MAX_OUTSTANDING" in ''|*[!0-9]*) echo "--max-outstanding must be a non-negative integer" >&2; exit 2 ;; esac

# `djinn_test_` is 11 characters, so the UUIDv7 hex starts at 1-based offset 12
# and its first 12 hex digits are the 48-bit millisecond timestamp. Ordering by
# datname is therefore ordering by creation time — oldest reaped first.
select_orphans_sql() {
  local age="$1"
  cat <<SQL
SELECT d.datname
  FROM pg_database d
 WHERE d.datname ~ '^djinn_test_[0-9a-f]{32}\$'
   AND to_timestamp((('x' || substr(d.datname, 12, 12))::bit(48)::bigint) / 1000.0)
       < now() - make_interval(secs => ${age})
   AND NOT EXISTS (
         SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname
       )
 ORDER BY d.datname
SQL
}

sweep() {
  local dropped=0 failed=0 db
  local orphans age="$MIN_AGE_SECONDS"
  if [ "$MAX_OUTSTANDING" -gt 0 ]; then
    # Count what is already finished and unreclaimed. Above the cap, fall back
    # to the floor age so the backlog is actually cleared instead of merely
    # being aged; see MAX_OUTSTANDING above for why that stays safe.
    local outstanding
    outstanding="$(psql "$PSQL_URL" --tuples-only --no-align --quiet \
      --command "$(select_orphans_sql "$MIN_AGE_FLOOR_SECONDS")" 2>/dev/null | grep -c . || true)"
    case "$outstanding" in ''|*[!0-9]*) outstanding=0 ;; esac
    if [ "$outstanding" -gt "$MAX_OUTSTANDING" ]; then
      age="$MIN_AGE_FLOOR_SECONDS"
      echo "djinn-test-db-reaper: ${outstanding} outstanding clones exceed cap ${MAX_OUTSTANDING}; reaping down to ${age}s"
    fi
  fi
  if ! orphans="$(psql "$PSQL_URL" --tuples-only --no-align --quiet --command "$(select_orphans_sql "$age")")"; then
    echo "djinn-test-db-reaper: could not query $PSQL_URL" >&2
    return 1
  fi
  for db in $orphans; do
    # No `WITH (FORCE)`: a database that acquired a session since the query
    # above must be left alone, and a plain DROP is what enforces that.
    if psql "$PSQL_URL" --quiet --command "DROP DATABASE \"$db\"" >/dev/null 2>&1; then
      dropped=$((dropped + 1))
    else
      failed=$((failed + 1))
    fi
  done
  echo "djinn-test-db-reaper: dropped=${dropped} skipped=${failed}"
  return 0
}

if [ "$LOOP" = "1" ]; then
  total=0
  while true; do
    line="$(sweep || true)"
    n="${line##*dropped=}"
    n="${n%% *}"
    case "$n" in ''|*[!0-9]*) n=0 ;; esac
    total=$((total + n))
    echo "${line} total=${total}"
    sleep "$INTERVAL_SECONDS"
  done
else
  sweep
fi
