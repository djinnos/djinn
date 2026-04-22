#!/usr/bin/env bash
# djinn-clang install.sh
#
# Installs clang + clangd from the distro package manager and scip-clang
# binary from the sourcegraph/scip-clang GitHub releases.

set -euo pipefail

VERSION="${VERSION:-22}"
INSTALL_DIR="/opt/djinn-clang"

log() { printf '[djinn-clang] %s\n' "$*"; }
warn() { printf '[djinn-clang][WARN] %s\n' "$*" >&2; }

detect_arch() {
    local uname_m
    uname_m="$(uname -m)"
    case "$uname_m" in
        x86_64|amd64) echo x86_64-linux ;;
        aarch64|arm64) echo aarch64-linux ;;
        *) echo "$uname_m-linux" ;;
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

install_clang() {
    local pm
    pm="$(detect_pkg_mgr)"
    case "$pm" in
        apt)
            export DEBIAN_FRONTEND=noninteractive
            apt-get update -y
            # `software-properties-common` ships `add-apt-repository`,
            # which the llvm.org installer invokes internally.
            apt-get install -y --no-install-recommends \
                ca-certificates curl gnupg lsb-release software-properties-common
            # Use llvm.org apt repo for the requested major version.
            curl -fsSL https://apt.llvm.org/llvm.sh -o /tmp/llvm.sh
            chmod +x /tmp/llvm.sh
            /tmp/llvm.sh "${VERSION}" all
            rm -f /tmp/llvm.sh
            apt-get install -y --no-install-recommends \
                "clangd-${VERSION}" || warn "clangd-${VERSION} not available; base clangd used"
            rm -rf /var/lib/apt/lists/*
            # Symlink versioned binaries to unversioned names.
            mkdir -p "${INSTALL_DIR}/bin"
            for tool in clang clang++ clangd clang-format clang-tidy; do
                if command -v "${tool}-${VERSION}" >/dev/null 2>&1; then
                    ln -sfn "$(command -v "${tool}-${VERSION}")" "${INSTALL_DIR}/bin/${tool}"
                fi
            done
            ;;
        apk)
            # Alpine ships one clang major version per release; we install whatever it provides.
            apk add --no-cache ca-certificates curl clang clang-extra-tools compiler-rt
            mkdir -p "${INSTALL_DIR}/bin"
            for tool in clang clang++ clangd; do
                if command -v "${tool}" >/dev/null 2>&1; then
                    ln -sfn "$(command -v "${tool}")" "${INSTALL_DIR}/bin/${tool}"
                fi
            done
            ;;
        dnf)
            dnf install -y ca-certificates curl clang clang-tools-extra
            dnf clean all
            mkdir -p "${INSTALL_DIR}/bin"
            for tool in clang clang++ clangd; do
                if command -v "${tool}" >/dev/null 2>&1; then
                    ln -sfn "$(command -v "${tool}")" "${INSTALL_DIR}/bin/${tool}"
                fi
            done
            ;;
        yum)
            yum install -y ca-certificates curl clang clang-tools-extra
            yum clean all
            ;;
        *)
            warn "unknown package manager; skipping clang install"
            ;;
    esac
}

install_scip_clang() {
    log "installing scip-clang"
    local arch
    arch="$(detect_arch)"
    # Release asset naming per sourcegraph/scip-clang: scip-clang-<arch>
    local url="https://github.com/sourcegraph/scip-clang/releases/latest/download/scip-clang-${arch}"
    mkdir -p "${INSTALL_DIR}/bin"
    if curl -fsSL "${url}" -o "${INSTALL_DIR}/bin/scip-clang"; then
        chmod +x "${INSTALL_DIR}/bin/scip-clang"
        log "scip-clang installed from ${url}"
    else
        warn "failed to download ${url}; scip-clang not installed"
    fi
}

persist_profile() {
    mkdir -p /etc/profile.d
    cat > /etc/profile.d/djinn-clang.sh <<EOF
export PATH="${INSTALL_DIR}/bin:\$PATH"
EOF
    chmod 0644 /etc/profile.d/djinn-clang.sh
}

main() {
    install_clang
    install_scip_clang
    persist_profile
    log "done"
}

main "$@"
