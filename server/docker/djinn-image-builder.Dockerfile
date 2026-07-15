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

# Cloud registry credential helpers. The build pod's `buildctl` reads
# `~/.docker/config.json` (sourced from the chart-rendered registry-auth
# Secret) which carries a `credHelpers` entry like
# `"<account>.dkr.ecr.<region>.amazonaws.com": "ecr-login"`. buildctl
# invokes the helper binary locally (not on buildkitd) to resolve a token
# per push, so the helper has to be in this image's PATH or every ECR
# push fails with `exec: "docker-credential-ecr-login": executable file
# not found in $PATH`. djinn-buildkitd bakes in the same helper for
# parity with the few buildkit code paths that auth server-side.
FROM alpine:3.20 AS helpers

ARG ECR_CRED_HELPER_VERSION=0.9.0
ARG TARGETARCH

RUN case "${TARGETARCH}" in amd64|arm64) ;; *) echo "unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; esac \
    && apk add --no-cache curl ca-certificates \
    && curl -fsSL --connect-timeout 10 --max-time 120 \
       --retry 4 --retry-delay 2 --retry-all-errors \
       -o /docker-credential-ecr-login \
       "https://amazon-ecr-credential-helper-releases.s3.amazonaws.com/${ECR_CRED_HELPER_VERSION}/linux-${TARGETARCH}/docker-credential-ecr-login" \
    && chmod 0755 /docker-credential-ecr-login

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

COPY --from=buildkit /usr/bin/buildctl /usr/local/bin/buildctl
COPY --from=helpers  /docker-credential-ecr-login /usr/local/bin/docker-credential-ecr-login
