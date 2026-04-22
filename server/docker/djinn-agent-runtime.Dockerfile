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

USER djinn
WORKDIR /workspace

# tini for correct PID 1 signal handling so `docker kill` / `kubectl delete
# pod` → SIGTERM → the worker can flush an in-flight terminal frame before
# exit. `tini` comes from the base image.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/djinn-agent-worker"]
