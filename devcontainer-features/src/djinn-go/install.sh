#!/usr/bin/env bash
# djinn-go install.sh
#
# Downloads Go from go.dev, installs gopls and scip-go via `go install`.

set -euo pipefail

VERSION="${VERSION:-1.26.2}"
export GOROOT="${GOROOT:-/usr/local/go}"
export GOPATH="${GOPATH:-/go}"
export PATH="${GOROOT}/bin:${GOPATH}/bin:${PATH}"

log() { printf '[djinn-go] %s\n' "$*"; }
warn() { printf '[djinn-go][WARN] %s\n' "$*" >&2; }

detect_arch() {
    local uname_m
    uname_m="$(uname -m)"
    case "$uname_m" in
        x86_64|amd64) echo amd64 ;;
        aarch64|arm64) echo arm64 ;;
        *) echo "$uname_m" ;;
    esac
}

detect_pkg_mgr() {
    if command -v apt-get >/dev/null 2>&1; then echo apt
    elif command -v apk >/dev/null 2>&1; then echo apk
    elif command -v dnf >/dev/null 2>&1; then echo dnf
    elif command -v yum >/dev/null 2>&1; then echo yum
    else echo unknown
    fi
}

install_os_deps() {
    local pm
    pm="$(detect_pkg_mgr)"
    case "$pm" in
        apt)
            export DEBIAN_FRONTEND=noninteractive
            apt-get update -y
            apt-get install -y --no-install-recommends ca-certificates curl git tar
            rm -rf /var/lib/apt/lists/*
            ;;
        apk) apk add --no-cache ca-certificates curl git tar ;;
        dnf) dnf install -y ca-certificates curl git tar && dnf clean all ;;
        yum) yum install -y ca-certificates curl git tar && yum clean all ;;
        *) warn "unknown package manager; assuming curl/tar/git present" ;;
    esac
}

download_go() {
    local arch
    arch="$(detect_arch)"
    local primary="https://go.dev/dl/go${VERSION}.linux-${arch}.tar.gz"
    local fallback="https://go.dev/dl/go${VERSION}.0.linux-${arch}.tar.gz"
    log "downloading ${primary}"
    if ! curl -fsSL "${primary}" -o /tmp/go.tar.gz; then
        warn "${primary} 404; trying ${fallback}"
        curl -fsSL "${fallback}" -o /tmp/go.tar.gz
    fi
    rm -rf "${GOROOT}"
    mkdir -p "$(dirname "${GOROOT}")"
    tar -C "$(dirname "${GOROOT}")" -xzf /tmp/go.tar.gz
    rm -f /tmp/go.tar.gz
    mkdir -p "${GOPATH}/bin"
    chmod -R a+rX "${GOROOT}" "${GOPATH}"
}

install_tools() {
    log "installing gopls"
    "${GOROOT}/bin/go" install golang.org/x/tools/gopls@latest
    log "installing scip-go"
    "${GOROOT}/bin/go" install github.com/sourcegraph/scip-go/cmd/scip-go@latest
    chmod -R a+rX "${GOPATH}"
}

persist_profile() {
    mkdir -p /etc/profile.d
    cat > /etc/profile.d/djinn-go.sh <<EOF
export GOROOT="${GOROOT}"
export GOPATH="${GOPATH}"
export PATH="${GOROOT}/bin:${GOPATH}/bin:\$PATH"
EOF
    chmod 0644 /etc/profile.d/djinn-go.sh
}

main() {
    install_os_deps
    download_go
    install_tools
    persist_profile
    log "done (go ${VERSION})"
}

main "$@"
