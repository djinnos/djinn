# syntax=docker/dockerfile:1.7
# djinn-server — control-plane runtime image.
#
# Packages a pre-compiled `djinn-server` binary (built by CI or
# `scripts/tilt/build-binaries.sh`) into a minimal Debian slim image.
# The binary carries the Vite-built SPA embedded via `rust-embed`, so
# this image hosts both the API (:3000) and the UI from the same port.
#
# Expected build context layout:
#   ./djinn-server           — release binary, already stripped
#
# Built by:
#   * scripts/tilt/wrap-server-image.sh  — local-dev fast path
#   * .github/workflows/release.yml      — GHCR publish

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive RUST_LOG=info

# ca-certificates for outbound HTTPS (LLM providers, GitHub App, OTLP),
# git for mirror clones, libssl3/openssl for crates that dynamically link
# OpenSSL, tini for correct PID 1 signal handling so `kubectl delete pod`
# → SIGTERM → graceful shutdown drains in-flight requests + flushes
# OTel spans.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates git libssl3 openssl tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 djinn \
    && useradd --system --uid 10001 --gid 10001 --home /home/djinn \
        --create-home --shell /usr/sbin/nologin djinn \
    && mkdir -p /var/lib/djinn/mirrors /var/lib/djinn/cache /var/lib/djinn/projects \
    && chown -R djinn:djinn /var/lib/djinn /home/djinn

COPY djinn-server /usr/local/bin/djinn-server
RUN chmod +x /usr/local/bin/djinn-server

EXPOSE 3000 8443
USER djinn
WORKDIR /home/djinn

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/djinn-server"]
