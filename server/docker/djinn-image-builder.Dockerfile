# djinn-image-builder — the container the image-controller (PR 5) schedules
# as a K8s Job to drive `devcontainer build` against a remote BuildKit
# daemon. Minimal surface:
#   * Node 22 for @devcontainers/cli
#   * docker CLI + buildx plugin for the remote-driver handshake
#   * git + curl + bash for the build script PR 5 ships
#
# Runtime command is supplied by the K8s Pod spec, not ENTRYPOINT — keeps
# this image reusable for ad-hoc `kubectl debug` sessions.

FROM node:22-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        docker.io \
        docker-buildx-plugin \
        bash \
        ca-certificates \
        git \
        curl \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g @devcontainers/cli
