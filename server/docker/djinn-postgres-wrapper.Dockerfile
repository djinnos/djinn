# ij6g: catalog Postgres wrapper image.
#
# One image that supervises the stock Postgres daemon AND the protocol-v1
# control server, so a single native sidecar provides both the loopback service
# and the private lease-provisioning socket. Debian (glibc) base matches the
# release binary linkage. Build context is server/docker/ (see
# build-wrapper-images.sh, which stages the pre-built binary here).
ARG POSTGRES_IMAGE=postgres:18
FROM ${POSTGRES_IMAGE}

COPY djinn-postgres-wrapper /usr/local/bin/djinn-postgres-wrapper
COPY catalog-wrapper/postgres-wrapper-entrypoint.sh /usr/local/bin/djinn-wrapper-entrypoint
RUN chmod +x /usr/local/bin/djinn-postgres-wrapper /usr/local/bin/djinn-wrapper-entrypoint

ENTRYPOINT ["/usr/local/bin/djinn-wrapper-entrypoint"]
