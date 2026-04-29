#!/usr/bin/env bash
# Install one or more Python versions via uv's standalone Python builds.
#
# Inputs (env):
#   PYTHON_VERSIONS      — required. Space-separated: "3.11 3.12".
#   DEFAULT_PYTHON       — optional. Defaults to the first PYTHON_VERSIONS entry.
#   SCIP_INDEXER         — optional. "scip-python" → installs the indexer
#                          at `${SCIP_PYTHON_VERSION}` (default = PyPI latest).
#   SCIP_PYTHON_VERSION  — optional. Pin scip-python to a specific PyPI
#                          version, e.g. `0.6.6`. Defaults to whatever
#                          `uv tool install scip-python` resolves on the
#                          build host (PyPI `latest`).
set -euo pipefail

: "${PYTHON_VERSIONS:?PYTHON_VERSIONS is required (space-separated majors, e.g. \"3.12\")}"
export UV_INSTALL_DIR="${UV_INSTALL_DIR:-/usr/local/uv}"
mkdir -p "${UV_INSTALL_DIR}"

curl --proto '=https' --tlsv1.2 -fsSL https://astral.sh/uv/install.sh \
    | env UV_INSTALL_DIR="${UV_INSTALL_DIR}" sh -s -- --no-modify-path

export PATH="${UV_INSTALL_DIR}:${PATH}"

for version in ${PYTHON_VERSIONS}; do
    uv python install "${version}"
done

DEFAULT_PYTHON_VALUE="${DEFAULT_PYTHON:-}"
if [ -z "${DEFAULT_PYTHON_VALUE}" ]; then
    # shellcheck disable=SC2086
    set -- ${PYTHON_VERSIONS}
    DEFAULT_PYTHON_VALUE="$1"
fi
uv python pin "${DEFAULT_PYTHON_VALUE}"

cat > /etc/profile.d/30-python.sh <<'EOF'
export UV_INSTALL_DIR=/usr/local/uv
export PATH="${UV_INSTALL_DIR}:${PATH}"
EOF
chmod 0644 /etc/profile.d/30-python.sh

if [ "${SCIP_INDEXER:-}" = "scip-python" ]; then
    if [ -n "${SCIP_PYTHON_VERSION:-}" ]; then
        uv tool install "scip-python==${SCIP_PYTHON_VERSION}"
    else
        uv tool install scip-python
    fi
fi
