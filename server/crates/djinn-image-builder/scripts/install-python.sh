#!/usr/bin/env bash
# Install one or more Python versions via uv's standalone Python builds.
#
# Inputs (env):
#   PYTHON_VERSIONS  — required. Space-separated: "3.11 3.12".
#   DEFAULT_PYTHON   — optional. Defaults to the first PYTHON_VERSIONS entry.
#   SCIP_INDEXER     — optional. "scip-python" → `uv pip install scip-python`
#                      into the default environment.
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
    uv tool install scip-python
fi
