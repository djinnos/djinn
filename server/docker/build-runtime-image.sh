#!/usr/bin/env bash
# Thin convenience wrapper around `docker build` for djinn-agent-runtime.
#
# Usage:
#   server/docker/build-runtime-image.sh            # tags :dev
#   server/docker/build-runtime-image.sh 0.1.0      # tags :0.1.0
#
# The build context is the repository root so the Dockerfile can `COPY server`
# without needing the whole tree — .dockerignore keeps target/ out.
set -euo pipefail

version="${1:-dev}"
image="djinn-agent-runtime:${version}"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

echo "building ${image} from ${repo_root}"
exec docker build \
    -f "${script_dir}/djinn-agent-runtime.Dockerfile" \
    -t "${image}" \
    "${repo_root}"
