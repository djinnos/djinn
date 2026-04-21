# djinn-image-builder — the container the image-controller (PR 5) schedules
# as a K8s Job to drive `devcontainer build` against a remote BuildKit
# daemon. Minimal surface:
#   * Node 22 for @devcontainers/cli
#   * docker CLI + buildx plugin for the remote-driver handshake
#     (docker-buildx-plugin is only published in Docker's official apt repo,
#      not Debian's, so we pull both the CE CLI and the plugin from there)
#   * git + curl + bash for the build script PR 5 ships
#
# Runtime command is supplied by the K8s Pod spec, not ENTRYPOINT — keeps
# this image reusable for ad-hoc `kubectl debug` sessions. The CLI talks to
# a remote buildkitd via DOCKER_HOST, so we intentionally do NOT install the
# docker engine/daemon.

FROM node:22-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        git \
        gnupg \
    && install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg \
        -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && . /etc/os-release \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian ${VERSION_CODENAME} stable" \
        > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        docker-ce-cli \
        docker-buildx-plugin \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g @devcontainers/cli
