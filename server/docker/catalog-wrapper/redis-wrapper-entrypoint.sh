#!/bin/sh
# ij6g: catalog Redis wrapper supervising entrypoint.
#
# Runs the stock Redis daemon on Pod loopback, waits until it answers PING, then
# execs the protocol-v1 control server. The control server binds the Unix socket
# named by CATALOG_CONTROL_SOCKET and provisions prefix-isolated per-attempt
# tenants against REDIS_WRAPPER_ADMIN_URL (both injected by the sidecar wiring).
set -eu

: "${CATALOG_CONTROL_SOCKET:?missing CATALOG_CONTROL_SOCKET}"
: "${REDIS_WRAPPER_ADMIN_URL:?missing REDIS_WRAPPER_ADMIN_URL}"

docker-entrypoint.sh redis-server --appendonly no &
DAEMON_PID=$!

i=0
until redis-cli -h 127.0.0.1 -p "${REDIS_PORT:-6379}" ping 2>/dev/null | grep -q PONG; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "catalog-wrapper: redis daemon exited during startup" >&2
        exit 1
    fi
    i=$((i + 1))
    if [ "$i" -ge 120 ]; then
        echo "catalog-wrapper: redis daemon did not become ready" >&2
        exit 1
    fi
    sleep 1
done

exec djinn-redis-wrapper
