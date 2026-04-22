#!/usr/bin/env bash
# Standalone build for djinn-agent-runtime. Chains the three Tilt scripts
# (base → binaries → wrap) without the registry push, so it works outside
# a kind + local-registry setup.
#
# Usage:
#   server/docker/build-runtime-image.sh            # tags :dev
#   server/docker/build-runtime-image.sh 0.1.0      # tags :0.1.0

set -euo pipefail

version="${1:-dev}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

bash "${repo_root}/scripts/tilt/build-agent-runtime-base.sh"
bash "${repo_root}/scripts/tilt/build-binaries.sh"
IMAGE_TAG="djinn-agent-runtime:${version}" SKIP_PUSH=1 \
    bash "${repo_root}/scripts/tilt/wrap-agent-runtime-image.sh"
