#!/usr/bin/env bash
set -euo pipefail

# GitHub's ubuntu-24.04 runner image includes PostgreSQL 16 but leaves the
# service disabled. Use that installation instead of pulling the same image in
# every matrix job before checkout, where neither retries nor Actions caches
# are available.
readonly expected_major=16
readonly port=5433
readonly database=djinn

installed_major="$(psql --version | sed -E 's/^psql \(PostgreSQL\) ([0-9]+).*/\1/')"
if [[ "$installed_major" != "$expected_major" ]]; then
  echo "::error title=Unexpected PostgreSQL version::ubuntu-24.04 must provide PostgreSQL ${expected_major}; found '${installed_major:-unknown}'"
  exit 1
fi
if ! pg_lsclusters --no-header | awk '{ print $1, $2 }' | grep -qx "${expected_major} main"; then
  echo "::error title=PostgreSQL cluster missing::Expected the ubuntu-24.04 runner to provide cluster '${expected_major} main'"
  pg_lsclusters || true
  exit 1
fi

sudo pg_ctlcluster "$expected_major" main start

# Durability-off tuning for the ephemeral CI cluster.
#
# ⚠ THROWAWAY DATA ONLY. `fsync=off`, `synchronous_commit=off` and
# `full_page_writes=off` mean an OS crash or power loss leaves an
# unrecoverable cluster. That is acceptable here and ONLY here: this script
# runs exclusively from `.github/workflows/*.yml` against the GitHub runner's
# bundled `postgresql@16-main` cluster, which is destroyed with the runner at
# the end of the job. Production Postgres is a completely separate Helm
# StatefulSet (`deploy/helm/djinn/templates/statefulset-postgres.yaml`) that
# passes its own `-c` args and never sources this file. Local dev/test
# Postgres is the `postgres-test` service in `docker-compose.yml`, which
# already runs with the same durability-off settings on tmpfs.
#
# The workload this targets: ~3.2k DB-backed tests, each of which clones the
# ~15MB `djinn_test_template` with `CREATE DATABASE … TEMPLATE` and drops it
# again. That is the most fsync- and WAL-heavy shape Postgres has, and it runs
# on the same runner disk as a ~400s cargo build and its target dir.
#
#   fsync/synchronous_commit/full_page_writes — mirror docker-compose.yml:
#     drop every durability barrier on a cluster whose data is disposable.
#     `fsync=off` is also the hard precondition for `CREATE DATABASE …
#     STRATEGY = FILE_COPY` in `server/crates/djinn-db/src/database.rs`:
#     FILE_COPY trades per-block WAL for a forced checkpoint before AND after
#     every clone, which is only a win once checkpoints are cheap. That code
#     probes `SHOW fsync` and declines FILE_COPY if this is ever reverted, so
#     the two cannot silently drift apart.
#   wal_level=minimal + max_wal_senders=0 — required as a pair (Postgres
#     refuses to start with minimal WAL and a non-zero walsender budget).
#     Nothing streams or archives this cluster's WAL, so there is no reason to
#     retain any.
#   autovacuum=off — nothing here lives long enough to need vacuuming, and
#     autovacuum workers otherwise chase thousands of short-lived databases.
#   shared_buffers/max_connections — match docker-compose.yml so the local and
#     CI clusters behave the same under the suite's connection fan-out.
sudo -u postgres psql --set=ON_ERROR_STOP=1 <<'SQL'
ALTER USER postgres WITH PASSWORD 'postgres';
ALTER SYSTEM SET port = '5433';
ALTER SYSTEM SET fsync = 'off';
ALTER SYSTEM SET synchronous_commit = 'off';
ALTER SYSTEM SET full_page_writes = 'off';
ALTER SYSTEM SET wal_level = 'minimal';
ALTER SYSTEM SET max_wal_senders = '0';
ALTER SYSTEM SET autovacuum = 'off';
ALTER SYSTEM SET shared_buffers = '512MB';
ALTER SYSTEM SET max_connections = '500';
SQL
sudo pg_ctlcluster "$expected_major" main restart

for attempt in {1..20}; do
  if pg_isready --host 127.0.0.1 --port "$port" --username postgres >/dev/null 2>&1; then
    break
  fi
  if [[ "$attempt" == 20 ]]; then
    echo "::error title=PostgreSQL startup timed out::PostgreSQL ${expected_major} did not become ready on port ${port}"
    sudo journalctl --unit "postgresql@${expected_major}-main.service" --no-pager --lines 100 || true
    exit 1
  fi
  sleep 1
done

# Assert the tuning is live on the RUNNING server, not merely recorded in
# postgresql.auto.conf. A silently-ignored setting (a future runner image
# shipping a conflicting postgresql.conf `include`, or a `wal_level`/
# `max_wal_senders` pair Postgres refuses and falls back on) would leave the
# suite untuned AND leave `STRATEGY = FILE_COPY` paying for checkpoints it
# cannot amortize. Fail the job instead.
declare -A expected_settings=(
  [fsync]=off
  [synchronous_commit]=off
  [full_page_writes]=off
  [wal_level]=minimal
  [max_wal_senders]=0
  [autovacuum]=off
)
for setting in "${!expected_settings[@]}"; do
  want="${expected_settings[$setting]}"
  got="$(sudo -u postgres psql --port "$port" --tuples-only --no-align \
    --command "SHOW ${setting}" 2>/dev/null || true)"
  if [[ "$got" != "$want" ]]; then
    echo "::error title=PostgreSQL tuning not applied::${setting} is '${got:-unset}', expected '${want}'"
    exit 1
  fi
done

if ! sudo -u postgres psql --port "$port" --tuples-only --no-align \
  --command "SELECT 1 FROM pg_database WHERE datname = '${database}'" | grep -qx 1; then
  sudo -u postgres createdb --port "$port" "$database"
fi

PGPASSWORD=postgres psql \
  --host 127.0.0.1 --port "$port" --username postgres --dbname "$database" \
  --set=ON_ERROR_STOP=1 --command 'SELECT version();'
