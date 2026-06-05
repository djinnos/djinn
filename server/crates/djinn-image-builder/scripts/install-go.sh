#!/usr/bin/env bash
# Install the requested Go toolchain version from the official tarball.
# Only one version at a time — Go's own multi-version support via
# `go install golang.org/dl/go1.22` suffices when workspaces differ.
#
# Inputs (env):
#   GO_VERSION       — required. e.g. "1.22".
#   SCIP_INDEXER     — optional. "scip-go" → installs the indexer at
#                      `${SCIP_GO_VERSION}` (default `latest`).
#   SCIP_GO_VERSION  — optional. Module-proxy version selector, e.g.
#                      `v0.2.3` or `latest`. Pin to a specific tag when
#                      `@latest` regresses (verified panic in v0.2.4 on
#                      certain Go monorepos — see project memory
#                      `project_complexity_feature.md`'s SCIP-go note).
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

export PATH="/usr/local/go/bin:${PATH}"

if [ "${SCIP_INDEXER:-}" = "scip-go" ]; then
    # scip-go moved from github.com/sourcegraph/scip-go to github.com/scip-code/scip-go;
    # fetching via the old path fails because v0.2.3's go.mod declares the new one.
    # GOBIN is pinned to /go/bin explicitly: the runtime image sets GOBIN=/cache/go/bin
    # (writable PVC) so the agent can `go install` at task time, but this baked indexer
    # must stay in the image layer at /go/bin or it would be hidden behind the empty
    # /cache PVC overlay at runtime.
    GOPATH=/go GOBIN=/go/bin go install "github.com/scip-code/scip-go/cmd/scip-go@${SCIP_GO_VERSION:-latest}"
fi

# gopls — the Go language server the agent's LSP manager drives. Bake it into
# /go/bin (on the image PATH) so resolve_binary() finds it on PATH and skips
# the session-time `go install` (which needs network + a writable $HOME). This
# is best-effort: a gopls/Go-toolchain version mismatch must NOT fail the whole
# image build — the LSP manager self-installs at runtime as a fallback (now
# that $HOME is writable). Pin via GOPLS_VERSION when @latest regresses against
# the project's Go version.
if ! GOPATH=/go GOBIN=/go/bin go install "golang.org/x/tools/gopls@${GOPLS_VERSION:-latest}"; then
    echo "[install-go] gopls install failed (non-fatal); LSP will self-install at runtime" >&2
fi

# --- protobuf codegen toolchain (protoc + the canonical Go plugins) ----------
# Most of a Go service's `go generate ./...` graph (ent, mockery, gqlgen, atlas)
# runs via `go run <module>`, which self-bootstraps from the module cache and
# needs nothing baked in. The exception is the proto step, which shells out to
# `protoc` with `--go_out`/`--go-grpc_out`. protoc is a C++ binary — not a Go
# module — so it can't be `go install`ed and must be installed here. Without it,
# regenerating committed `*.pb.go` after a `.proto` edit is impossible and agents
# resort to hand-authoring descriptor bytes (the failure mode that reopened a
# batch-payments gRPC change twice). Fatal on failure by design: unlike gopls,
# there is no runtime self-heal for protoc, so a silently-absent protoc just
# reintroduces the bug. Versions are pinnable via PROTOC_VERSION /
# PROTOC_GEN_GO_VERSION / PROTOC_GEN_GO_GRPC_VERSION.
case "${arch}" in
    x86_64)  protoc_asset_arch="x86_64" ;;
    aarch64) protoc_asset_arch="aarch_64" ;;  # protoc release assets spell it "aarch_64"
esac

protoc_version="${PROTOC_VERSION:-}"
if [ -z "${protoc_version}" ]; then
    # Resolve the latest protoc release tag (vXX.Y -> XX.Y); fall back to a known
    # good pin if the GitHub API is unreachable/rate-limited during the build.
    protoc_version="$(
        curl --proto '=https' --tlsv1.2 -fsSL \
            'https://api.github.com/repos/protocolbuffers/protobuf/releases/latest' \
        | grep -oE '"tag_name":[[:space:]]*"v[0-9.]+"' \
        | head -n1 \
        | sed -E 's/.*"v([0-9.]+)".*/\1/' \
        || true
    )"
    [ -n "${protoc_version}" ] || protoc_version="29.3"
fi

if ! command -v unzip >/dev/null 2>&1; then
    apt-get update
    apt-get install -y --no-install-recommends unzip
    rm -rf /var/lib/apt/lists/*
fi

protoc_zip="$(mktemp)"
curl --proto '=https' --tlsv1.2 -fsSL \
    "https://github.com/protocolbuffers/protobuf/releases/download/v${protoc_version}/protoc-${protoc_version}-linux-${protoc_asset_arch}.zip" \
    -o "${protoc_zip}"
# bin/protoc -> /usr/local/bin/protoc; include/ ships the well-known-type protos
# (google/protobuf/*.proto) that protoc resolves relative to its own bin dir.
unzip -o "${protoc_zip}" 'bin/protoc' 'include/*' -d /usr/local
chmod 0755 /usr/local/bin/protoc
rm -f "${protoc_zip}"
echo "[install-go] installed protoc ${protoc_version}" >&2

# protoc execs these as `protoc-gen-go` / `protoc-gen-go-grpc` via PATH lookup
# (triggered by --go_out / --go-grpc_out). They ARE Go modules, so install them
# into /go/bin (already on the image PATH via 40-go.sh).
GOPATH=/go GOBIN=/go/bin go install \
    "google.golang.org/protobuf/cmd/protoc-gen-go@${PROTOC_GEN_GO_VERSION:-latest}"
GOPATH=/go GOBIN=/go/bin go install \
    "google.golang.org/grpc/cmd/protoc-gen-go-grpc@${PROTOC_GEN_GO_GRPC_VERSION:-latest}"
