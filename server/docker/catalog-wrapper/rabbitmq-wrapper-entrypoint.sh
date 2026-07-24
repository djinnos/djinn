#!/bin/sh
# ij6g: catalog RabbitMQ wrapper supervising entrypoint.
#
# Runs the stock RabbitMQ broker on Pod loopback, waits until it finishes
# startup, then execs the protocol-v1 control server. The control server binds
# the Unix socket named by CATALOG_CONTROL_SOCKET, provisions vhost-isolated
# per-attempt tenants over RABBITMQ_WRAPPER_AMQP_URL, and drives lifecycle via
# the local rabbitmqctl in this image (both injected by the sidecar wiring).
set -eu

: "${CATALOG_CONTROL_SOCKET:?missing CATALOG_CONTROL_SOCKET}"
: "${RABBITMQ_WRAPPER_AMQP_URL:?missing RABBITMQ_WRAPPER_AMQP_URL}"

docker-entrypoint.sh rabbitmq-server &
DAEMON_PID=$!

i=0
until rabbitmqctl await_startup >/dev/null 2>&1; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "catalog-wrapper: rabbitmq broker exited during startup" >&2
        exit 1
    fi
    i=$((i + 1))
    if [ "$i" -ge 120 ]; then
        echo "catalog-wrapper: rabbitmq broker did not become ready" >&2
        exit 1
    fi
    sleep 2
done

exec djinn-rabbitmq-wrapper
