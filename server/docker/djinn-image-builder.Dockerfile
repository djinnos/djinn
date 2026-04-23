# djinn-image-builder — the container the image-controller schedules as a
# K8s Job to run `buildctl build` against the in-cluster BuildKit daemon.
#
# Needs:
#   * buildctl (the BuildKit client, matched to the daemon's v0.15 release)
#   * bash (the scheduled command uses `set -euo pipefail`)
#   * ca-certificates (TLS trust for Zot / registry auth)
#
# Runtime command is supplied by the K8s Pod spec, not ENTRYPOINT — keeps
# this image reusable for ad-hoc `kubectl debug` sessions. The CLI talks
# to buildkitd over gRPC via DOCKER_HOST, so we do NOT install the docker
# engine/daemon.
#
# Pre-2026-04-22 this image carried Node + @devcontainers/cli +
# docker-ce-cli + docker-buildx-plugin because the old design drove
# `devcontainer build`. The env-config cut-over replaced that with a
# djinn-native Dockerfile generator + buildctl, so all of that tooling
# was dead weight and the Dockerfile was rewritten to match.

FROM moby/buildkit:v0.15.2 AS buildkit

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

COPY --from=buildkit /usr/bin/buildctl /usr/local/bin/buildctl
