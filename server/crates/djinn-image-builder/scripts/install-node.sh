#!/usr/bin/env bash
# Install one or more Node versions via fnm, plus the requested package
# managers as global npm packages on the first version.
#
# Inputs (env):
#   NODE_VERSIONS     — required. Space-separated major versions: "20 22".
#                       fnm resolves each to the latest LTS in the major.
#   PACKAGE_MANAGERS  — optional. Space-separated: "pnpm yarn".
#   DEFAULT_NODE      — optional. Defaults to the first NODE_VERSIONS entry.
#
# fnm is installed at /usr/local/share/fnm and managed via a PATH
# fragment at /etc/profile.d/20-node.sh.
set -euo pipefail

: "${NODE_VERSIONS:?NODE_VERSIONS is required (space-separated majors)}"
export FNM_DIR="${FNM_DIR:-/usr/local/share/fnm}"
mkdir -p "${FNM_DIR}"

curl --proto '=https' --tlsv1.2 -fsSL https://fnm.vercel.app/install \
    | bash -s -- --install-dir "${FNM_DIR}" --skip-shell

export PATH="${FNM_DIR}:${PATH}"

DEFAULT_NODE_VALUE="${DEFAULT_NODE:-}"
if [ -z "${DEFAULT_NODE_VALUE}" ]; then
    # shellcheck disable=SC2086
    set -- ${NODE_VERSIONS}
    DEFAULT_NODE_VALUE="$1"
fi

for version in ${NODE_VERSIONS}; do
    fnm install "${version}"
done
fnm default "${DEFAULT_NODE_VALUE}"

# The generated Dockerfile's canonical PATH has /opt/node/bin (matches the
# agent-runtime base image's layout, which untars node directly there). fnm
# installs to FNM_DIR/node-versions/vX.Y.Z/installation and points its
# `default` alias at the active one — symlink /opt/node through that alias
# so `/opt/node/bin/node` resolves without per-shell fnm activation.
if [ -L "${FNM_DIR}/aliases/default" ]; then
    ln -sfn "${FNM_DIR}/aliases/default" /opt/node
fi

# Expose fnm + the default Node version on every shell.
cat > /etc/profile.d/20-node.sh <<'EOF'
export FNM_DIR=/usr/local/share/fnm
export PATH="${FNM_DIR}:${PATH}"
if command -v fnm >/dev/null 2>&1; then
    eval "$(fnm env --shell bash)"
fi
EOF
chmod 0644 /etc/profile.d/20-node.sh

# Global package managers. Install on the default node only — each
# major has its own node_modules tree, so we don't try to replicate
# across versions.
if [ -n "${PACKAGE_MANAGERS:-}" ]; then
    eval "$(fnm env --shell bash)"
    fnm use "${DEFAULT_NODE_VALUE}"
    for pm in ${PACKAGE_MANAGERS}; do
        case "${pm}" in
            pnpm|yarn|bun|npm)
                npm install -g "${pm}"
                ;;
            *)
                echo "[install-node] unknown package manager '${pm}'; skipping" >&2
                ;;
        esac
    done
fi
