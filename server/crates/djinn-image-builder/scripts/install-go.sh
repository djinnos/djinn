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

# go.dev only publishes patch-specific tarballs (e.g. go1.22.12, never bare
# go1.22). go.mod typically records only a major.minor, so resolve to the
# newest matching patch via the download index. `include=all` covers EOL
# series (the default listing drops anything older than the two live ones).
if [[ "${GO_VERSION}" =~ ^[0-9]+\.[0-9]+$ ]]; then
    # `|| true` is required because `head -n1` closes its stdin after the
    # first match, sending SIGPIPE back through grep to curl. With the
    # outer script's `set -o pipefail` that would propagate up and abort
    # us with exit 141 before we ever got to read `resolved`. Swallow the
    # pipeline status; the `-n "$resolved"` check below is the real gate.
    resolved="$(
        curl --proto '=https' --tlsv1.2 -fsSL 'https://go.dev/dl/?mode=json&include=all' \
        | grep -oE "\"version\":[[:space:]]*\"go${GO_VERSION}\\.[0-9]+\"" \
        | head -n1 \
        | sed -E 's/.*"go([0-9.]+)".*/\1/' \
        || true
    )"
    if [ -n "${resolved}" ]; then
        echo "[install-go] resolved ${GO_VERSION} -> ${resolved}" >&2
        GO_VERSION="${resolved}"
    else
        echo "[install-go] could not resolve latest patch for ${GO_VERSION} via go.dev" >&2
        exit 1
    fi
fi

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
    # scip-go moved from github.com/sourcegraph/scip-go to github.com/scip-code/scip-go;
    # fetching via the old path fails because v0.2.3's go.mod declares the new one.
    GOPATH=/go go install github.com/scip-code/scip-go/cmd/scip-go@latest
fi
