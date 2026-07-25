#!/bin/sh
# ij6g: catalog Postgres wrapper supervising entrypoint.
#
# Runs the stock Postgres daemon on Pod loopback via the official image
# entrypoint, waits until it accepts connections, then execs the protocol-v1
# control server. The control server binds the Unix socket named by
# CATALOG_CONTROL_SOCKET and provisions fresh per-attempt tenants against
# POSTGRES_WRAPPER_ADMIN_URL (both injected by the djinn sidecar wiring).
set -eu

: "${CATALOG_CONTROL_SOCKET:?missing CATALOG_CONTROL_SOCKET}"
: "${POSTGRES_WRAPPER_ADMIN_URL:?missing POSTGRES_WRAPPER_ADMIN_URL}"

docker-entrypoint.sh postgres &
DAEMON_PID=$!

# Bounded readiness wait so a wedged daemon fails the sidecar startup probe
# rather than hanging forever.
i=0
until pg_isready -h 127.0.0.1 -p "${PGPORT:-5432}" -U "${POSTGRES_USER:-postgres}" >/dev/null 2>&1; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "catalog-wrapper: postgres daemon exited during startup" >&2
        exit 1
    fi
    i=$((i + 1))
    if [ "$i" -ge 120 ]; then
        echo "catalog-wrapper: postgres daemon did not become ready" >&2
        exit 1
    fi
    sleep 1
done

exec djinn-postgres-wrapper
