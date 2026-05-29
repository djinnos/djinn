#!/usr/bin/env bash
# Install one or more Python versions via uv's standalone Python builds.
#
# Inputs (env):
#   PYTHON_VERSIONS      — required. Space-separated: "3.11 3.12".
#   DEFAULT_PYTHON       — optional. Defaults to the first PYTHON_VERSIONS entry.
#   SCIP_INDEXER         — optional. "scip-python" → installs the indexer
#                          (`@sourcegraph/scip-python`) at
#                          `${SCIP_PYTHON_VERSION}` via npm.
#   SCIP_PYTHON_VERSION  — optional. Pin scip-python to a specific npm dist
#                          version, e.g. `0.6.6`. `latest` (the default) maps
#                          to the npm `@latest` dist-tag.
#
# NOTE on the indexer: scip-python is NOT a PyPI package — it is published
# only as the npm package `@sourcegraph/scip-python`, whose CLI bin is a
# `#!/usr/bin/env node` script. So the indexer needs a Node runtime both to
# install (npm) and to run (the warm-graph job execs `scip-python` directly).
# A Python-only project image has no Node from install-node.sh, so we bring a
# minimal Node along here when (and only when) the indexer is requested. The
# whole indexer step is best-effort: a SCIP hiccup must not fail the image
# build (the indexer only feeds graph warming, never task execution) — mirrors
# install-go.sh's non-fatal gopls install.
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
    # --- ensure a Node runtime at /opt/node (matches the image PATH) -------
    # A polyglot project may already have Node from install-node.sh (it runs
    # before this block); reuse it. Otherwise fetch a minimal Node tarball,
    # the same way the agent-runtime base image does.
    if ! command -v node >/dev/null 2>&1 && [ ! -x /opt/node/bin/node ]; then
        NODE_VERSION="${NODE_VERSION:-20.17.0}"
        arch="$(uname -m)"
        case "${arch}" in
            x86_64)  node_arch="x64" ;;
            aarch64) node_arch="arm64" ;;
            *) node_arch="" ; echo "[install-python] unsupported arch ${arch}; skipping scip-python" >&2 ;;
        esac
        if [ -n "${node_arch}" ]; then
            mkdir -p /opt/node
            if curl --proto '=https' --tlsv1.2 -fsSL \
                "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${node_arch}.tar.xz" \
                | tar -xJf - -C /opt/node --strip-components=1; then
                cat > /etc/profile.d/25-node-scip.sh <<'EOF'
export PATH="/opt/node/bin:${PATH}"
EOF
                chmod 0644 /etc/profile.d/25-node-scip.sh
            else
                echo "[install-python] node download failed (non-fatal); skipping scip-python" >&2
            fi
        fi
    fi

    # --- install the indexer via npm (non-fatal) --------------------------
    if command -v node >/dev/null 2>&1 || [ -x /opt/node/bin/node ]; then
        export PATH="/opt/node/bin:${PATH}"
        if [ -n "${SCIP_PYTHON_VERSION:-}" ] && [ "${SCIP_PYTHON_VERSION}" != "latest" ]; then
            scip_pkg="@sourcegraph/scip-python@${SCIP_PYTHON_VERSION}"
        else
            scip_pkg="@sourcegraph/scip-python"
        fi
        if ! npm install -g "${scip_pkg}"; then
            echo "[install-python] scip-python install failed (non-fatal); Python graph warming will be skipped" >&2
        fi
    fi
fi
