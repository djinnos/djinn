# ij6g: catalog RabbitMQ wrapper image.
#
# One image that supervises the stock RabbitMQ broker AND the protocol-v1
# control server; the broker's own `rabbitmqctl` drives vhost/user lifecycle.
# Debian (glibc) base matches the release binary linkage. Build context is
# server/docker/ (build-wrapper-images.sh stages the pre-built binary here).
ARG RABBITMQ_IMAGE=rabbitmq:4
FROM ${RABBITMQ_IMAGE}

COPY djinn-rabbitmq-wrapper /usr/local/bin/djinn-rabbitmq-wrapper
COPY catalog-wrapper/rabbitmq-wrapper-entrypoint.sh /usr/local/bin/djinn-wrapper-entrypoint
RUN chmod +x /usr/local/bin/djinn-rabbitmq-wrapper /usr/local/bin/djinn-wrapper-entrypoint

ENTRYPOINT ["/usr/local/bin/djinn-wrapper-entrypoint"]
