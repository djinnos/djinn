#!/usr/bin/env bash
# Install the requested Go toolchain version from the official tarball.
# Only one version at a time — Go's own multi-version support via
# `go install golang.org/dl/go1.22` suffices when workspaces differ.
#
# Inputs (env):
#   GO_VERSION   — required. e.g. "1.22".
#   SCIP_INDEXER — optional. "scip-go" → `go install github.com/sourcegraph/scip-go/cmd/scip-go@latest`.
set -euo pipefail

: "${GO_VERSION:?GO_VERSION is required, e.g. \"1.22\"}"

arch="$(uname -m)"
case "${arch}" in
    x86_64)  goarch="amd64" ;;
    aarch64) goarch="arm64" ;;
    *) echo "[install-go] unsupported arch ${arch}" >&2; exit 1 ;;
esac

url="https://go.dev/dl/go${GO_VERSION}.linux-${goarch}.tar.gz"
curl --proto '=https' --tlsv1.2 -fsSL "${url}" | tar -C /usr/local -xzf -

cat > /etc/profile.d/40-go.sh <<'EOF'
export PATH="/usr/local/go/bin:${PATH}"
export GOPATH="${GOPATH:-/go}"
export PATH="${GOPATH}/bin:${PATH}"
EOF
chmod 0644 /etc/profile.d/40-go.sh
mkdir -p /go/bin

if [ "${SCIP_INDEXER:-}" = "scip-go" ]; then
    export PATH="/usr/local/go/bin:${PATH}"
    GOPATH=/go go install github.com/sourcegraph/scip-go/cmd/scip-go@latest
fi
