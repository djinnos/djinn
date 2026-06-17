#!/usr/bin/env bash
# Install one or more Node versions via fnm, plus the requested package
# managers as global npm packages on the first version.
#
# Inputs (env):
#   NODE_VERSIONS           — required. Space-separated major versions: "20 22".
#                             fnm resolves each to the latest LTS in the major.
#   PACKAGE_MANAGERS        — optional. Space-separated: "pnpm yarn".
#   DEFAULT_NODE            — optional. Defaults to the first NODE_VERSIONS entry.
#   SCIP_INDEXER            — optional. "scip-typescript" → installs the indexer
#                             at `${SCIP_TYPESCRIPT_VERSION}` (default `latest`)
#                             via npm on the default node. Handles JS too —
#                             scip-typescript is a strict superset.
#   SCIP_TYPESCRIPT_VERSION — optional. Pin scip-typescript to a specific npm
#                             version (e.g. `0.3.30`) when `latest` regresses.
#
# fnm is installed at /usr/local/share/fnm and managed via a PATH
# fragment at /etc/profile.d/20-node.sh.
set -euo pipefail

: "${NODE_VERSIONS:?NODE_VERSIONS is required (space-separated majors)}"
export FNM_DIR="${FNM_DIR:-/usr/local/share/fnm}"
mkdir -p "${FNM_DIR}"

# Isolate this build's fnm "multishell" symlinks to a throwaway dir. The build
# runs as root, and the build-time `fnm env` calls below would otherwise mint a
# multishell symlink under $HOME/.local/state/fnm_multishells — leaving that
# tree ROOT-OWNED in the (non-root) worker's home, which then makes every worker
# login shell fail EACCES when it tries to create its own per-shell symlink.
# Pointing XDG_STATE_HOME at /tmp for the build keeps the worker's home clean so
# the worker creates (and owns) its own state dir at runtime.
export XDG_STATE_HOME=/tmp/djinn-build-fnm-state
mkdir -p "${XDG_STATE_HOME}"

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
#
# `fnm env` mints a per-shell "multishell" symlink under
# $XDG_STATE_HOME/fnm_multishells ($HOME/.local/state/... by default). If that
# dir isn't writable by the current user, `fnm env` fails with EACCES. The
# activation MUST be non-fatal: node also resolves via /opt/node/bin (on PATH
# from the symlink above), so a failed `fnm env` must never abort the login
# shell or leak a non-zero status — otherwise it wedges unrelated commands like
# `cargo` (the shell init dies before the command runs). Capture stdout only,
# eval only on success, and always return 0.
cat > /etc/profile.d/20-node.sh <<'EOF'
export FNM_DIR=/usr/local/share/fnm
export PATH="${FNM_DIR}:${PATH}"
if command -v fnm >/dev/null 2>&1; then
    __djinn_fnm_env="$(fnm env --shell bash 2>/dev/null)" && eval "${__djinn_fnm_env}" || true
    unset __djinn_fnm_env
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

if [ "${SCIP_INDEXER:-}" = "scip-typescript" ]; then
    eval "$(fnm env --shell bash)"
    fnm use "${DEFAULT_NODE_VALUE}"
    if [ -n "${SCIP_TYPESCRIPT_VERSION:-}" ] && [ "${SCIP_TYPESCRIPT_VERSION}" != "latest" ]; then
        npm install -g "@sourcegraph/scip-typescript@${SCIP_TYPESCRIPT_VERSION}"
    else
        npm install -g @sourcegraph/scip-typescript
    fi
fi

# Defensive: hand the worker its XDG state dir back. If any earlier layer (base
# image or a prior build) left $HOME/.local/state root-owned, the non-root
# worker can't write its per-shell fnm multishell symlink and `fnm env` fails on
# every login. Drop any build-created multishell tree and chown the worker's
# .local to whoever owns the home dir. Guarded so it's a no-op on images without
# the standard /home/djinn worker home.
rm -rf /tmp/djinn-build-fnm-state 2>/dev/null || true
if [ -d /home/djinn ]; then
    rm -rf /home/djinn/.local/state/fnm_multishells 2>/dev/null || true
    mkdir -p /home/djinn/.local/state
    chown -R --reference=/home/djinn /home/djinn/.local
fi
