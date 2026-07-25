# ij6g: catalog Redis wrapper image.
#
# One image that supervises the stock Redis daemon AND the protocol-v1 control
# server. Debian (glibc) base matches the release binary linkage. Build context
# is server/docker/ (build-wrapper-images.sh stages the pre-built binary here).
ARG REDIS_IMAGE=redis:7
FROM ${REDIS_IMAGE}

COPY djinn-redis-wrapper /usr/local/bin/djinn-redis-wrapper
COPY catalog-wrapper/redis-wrapper-entrypoint.sh /usr/local/bin/djinn-wrapper-entrypoint
RUN chmod +x /usr/local/bin/djinn-redis-wrapper /usr/local/bin/djinn-wrapper-entrypoint

ENTRYPOINT ["/usr/local/bin/djinn-wrapper-entrypoint"]
