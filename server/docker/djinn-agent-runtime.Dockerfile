# syntax=docker/dockerfile:1.7
# djinn-agent-runtime — per-task-run sandbox image.
#
# This is the thin top layer: it lays the `djinn-agent-worker` binary on top
# of `djinn-agent-runtime-base`, which carries all the slow-churning bits
# (LSPs, rustup, sccache, mold, apt deps). Rebuilds on every worker-source
# change are fast — just a binary copy — because the heavy layers are cached
# in the base image.
#
# Expected build context (produced by `scripts/tilt/wrap-agent-runtime-image.sh`):
#   ./djinn-agent-worker  — the release binary, already compiled and stripped
#                           by the host-side `build-binaries.sh` cargo pass.
#
# The base image is referenced by local tag (`djinn-agent-runtime-base:dev`)
# and must exist in the local Docker image store when this Dockerfile is
# built. The Tilt pipeline enforces that via `resource_deps`.

ARG BASE_IMAGE=djinn-agent-runtime-base:dev
FROM ${BASE_IMAGE}

COPY djinn-agent-worker /usr/local/bin/djinn-agent-worker
RUN chmod +x /usr/local/bin/djinn-agent-worker

# The mandatory cgroup-launcher sidecar runs from the SAME image as the worker
# with a different entrypoint (see djinn-k8s::launcher). Ship its binary here at
# /usr/local/bin; the image-builder copies it on to /opt/djinn/bin in the
# per-project image so the rendered launcher command resolves to a real artifact.
COPY djinn-cgroup-launcher /usr/local/bin/djinn-cgroup-launcher
RUN chmod +x /usr/local/bin/djinn-cgroup-launcher

# Seed the cache-backed RUSTUP_HOME from the build-time install on first start.
# The base image installs rustup into $RUSTUP_SEED_DIR (=/usr/local/rustup,
# read-only for the djinn user) so the heavy layer is shared and immutable.
# At runtime RUSTUP_HOME=/cache/rustup so the djinn user can install
# workspace-pinned toolchains (e.g. `1.94.1` declared by a repo's
# rust-toolchain.toml) without falling back to writing into the workspace
# itself — that fallback was committing `.rustup/toolchains/**` and
# `.cargo/bin/rustup` as diff junk, blocking reviewer approval on every
# worker PR.
#
# `cp -an` is no-clobber, so subsequent pod starts on a populated PVC are
# a cheap stat. The seed has to happen at runtime (not in the image) because
# /cache is a PVC mount, not part of the image layers.
RUN cat > /usr/local/bin/djinn-agent-entrypoint.sh <<'EOF'
#!/bin/sh
set -e
if [ -n "${RUSTUP_HOME:-}" ] && [ -n "${RUSTUP_SEED_DIR:-}" ] \
        && [ ! -f "${RUSTUP_HOME}/settings.toml" ] \
        && [ -d "${RUSTUP_SEED_DIR}" ]; then
    cp -an "${RUSTUP_SEED_DIR}/." "${RUSTUP_HOME}/" 2>/dev/null || true
fi
exec /usr/local/bin/djinn-agent-worker "$@"
EOF
RUN chmod +x /usr/local/bin/djinn-agent-entrypoint.sh

USER djinn
WORKDIR /workspace

# tini for correct PID 1 signal handling so `docker kill` / `kubectl delete
# pod` → SIGTERM → the worker can flush an in-flight terminal frame before
# exit. `tini` comes from the base image.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/djinn-agent-entrypoint.sh"]
