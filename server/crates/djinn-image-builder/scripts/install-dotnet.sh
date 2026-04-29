#!/usr/bin/env bash
# Install the requested .NET SDK via the official dotnet-install script.
# Skeleton — no .NET consumer today.
#
# Inputs (env):
#   DOTNET_VERSION       — required. e.g. "8.0".
#   SCIP_INDEXER         — optional. "scip-dotnet" → installs the indexer
#                          at `${SCIP_DOTNET_VERSION}` (default `latest`)
#                          via `dotnet tool install --global`.
#   SCIP_DOTNET_VERSION  — optional. Pin scip-dotnet to a specific NuGet
#                          version, e.g. `0.4.0`.
set -euo pipefail

: "${DOTNET_VERSION:?DOTNET_VERSION is required, e.g. \"8.0\"}"

export DOTNET_ROOT="${DOTNET_ROOT:-/usr/local/dotnet}"
mkdir -p "${DOTNET_ROOT}"

curl --proto '=https' --tlsv1.2 -fsSL https://dot.net/v1/dotnet-install.sh -o /tmp/dotnet-install.sh
chmod +x /tmp/dotnet-install.sh
/tmp/dotnet-install.sh --channel "${DOTNET_VERSION}" --install-dir "${DOTNET_ROOT}"
rm -f /tmp/dotnet-install.sh

# Profile fragment carries DOTNET_ROOT plus the global-tools directory.
# `dotnet tool install --global` drops binaries under
# $HOME/.dotnet/tools — adding it to PATH at the profile level means
# any future tool install lands on PATH without further script tweaks
# (the alternative — symlinking each binary into /usr/local/bin —
# would be per-tool and miss anything installed at runtime).
cat > /etc/profile.d/70-dotnet.sh <<'EOF'
export DOTNET_ROOT=/usr/local/dotnet
export PATH="${DOTNET_ROOT}:${HOME}/.dotnet/tools:${PATH}"
EOF
chmod 0644 /etc/profile.d/70-dotnet.sh

if [ "${SCIP_INDEXER:-}" = "scip-dotnet" ]; then
    export PATH="${DOTNET_ROOT}:${PATH}"
    export DOTNET_CLI_TELEMETRY_OPTOUT=1
    if [ -n "${SCIP_DOTNET_VERSION:-}" ] && [ "${SCIP_DOTNET_VERSION}" != "latest" ]; then
        dotnet tool install --global --version "${SCIP_DOTNET_VERSION}" scip-dotnet
    else
        dotnet tool install --global scip-dotnet
    fi
    # `--global` installs into $HOME/.dotnet/tools — root's home at
    # build time. Symlink into /usr/local/bin so the binary is on PATH
    # for every UID at runtime, even before /etc/profile.d sources.
    if [ -x "${HOME}/.dotnet/tools/scip-dotnet" ]; then
        ln -sf "${HOME}/.dotnet/tools/scip-dotnet" /usr/local/bin/scip-dotnet
    fi
fi
