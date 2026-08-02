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
sudo -u postgres psql --set=ON_ERROR_STOP=1 <<'SQL'
ALTER USER postgres WITH PASSWORD 'postgres';
ALTER SYSTEM SET port = '5433';
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

if ! sudo -u postgres psql --port "$port" --tuples-only --no-align \
  --command "SELECT 1 FROM pg_database WHERE datname = '${database}'" | grep -qx 1; then
  sudo -u postgres createdb --port "$port" "$database"
fi

PGPASSWORD=postgres psql \
  --host 127.0.0.1 --port "$port" --username postgres --dbname "$database" \
  --set=ON_ERROR_STOP=1 --command 'SELECT version();'
