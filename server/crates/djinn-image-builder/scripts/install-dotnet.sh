#!/usr/bin/env bash
# Install the requested .NET SDK via the official dotnet-install script.
# Skeleton — no .NET consumer today.
#
# Inputs (env):
#   DOTNET_VERSION — required. e.g. "8.0".
set -euo pipefail

: "${DOTNET_VERSION:?DOTNET_VERSION is required, e.g. \"8.0\"}"

export DOTNET_ROOT="${DOTNET_ROOT:-/usr/local/dotnet}"
mkdir -p "${DOTNET_ROOT}"

curl --proto '=https' --tlsv1.2 -fsSL https://dot.net/v1/dotnet-install.sh -o /tmp/dotnet-install.sh
chmod +x /tmp/dotnet-install.sh
/tmp/dotnet-install.sh --channel "${DOTNET_VERSION}" --install-dir "${DOTNET_ROOT}"
rm -f /tmp/dotnet-install.sh

cat > /etc/profile.d/70-dotnet.sh <<'EOF'
export DOTNET_ROOT=/usr/local/dotnet
export PATH="${DOTNET_ROOT}:${PATH}"
EOF
chmod 0644 /etc/profile.d/70-dotnet.sh
