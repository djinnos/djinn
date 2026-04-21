#!/usr/bin/env bash
# djinn-dotnet install.sh
#
# Installs the .NET SDK via Microsoft's dotnet-install.sh, csharp-ls as a global
# tool, and scip-dotnet as a global tool.

set -euo pipefail

SDK_VERSION="${SDK_VERSION:-10.0}"
export DOTNET_ROOT="${DOTNET_ROOT:-/usr/local/dotnet}"
export DOTNET_TOOLS="${DOTNET_TOOLS:-/usr/local/dotnet-tools}"
export PATH="${DOTNET_ROOT}:${DOTNET_TOOLS}:${PATH}"
export DOTNET_CLI_TELEMETRY_OPTOUT=1

log() { printf '[djinn-dotnet] %s\n' "$*"; }
warn() { printf '[djinn-dotnet][WARN] %s\n' "$*" >&2; }

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
            apt-get install -y --no-install-recommends \
                ca-certificates curl libicu-dev libssl-dev libunwind-dev \
                liblttng-ust-dev libkrb5-dev zlib1g
            rm -rf /var/lib/apt/lists/*
            ;;
        apk)
            apk add --no-cache ca-certificates curl icu-libs \
                krb5-libs libgcc libintl libssl3 libstdc++ zlib
            ;;
        dnf)
            dnf install -y ca-certificates curl libicu krb5-libs openssl-libs \
                zlib lttng-ust libunwind
            dnf clean all
            ;;
        yum)
            yum install -y ca-certificates curl libicu krb5-libs openssl-libs \
                zlib lttng-ust libunwind
            yum clean all
            ;;
        *)
            warn "unknown package manager; assuming icu/krb5 libs present"
            ;;
    esac
}

install_dotnet_sdk() {
    mkdir -p "${DOTNET_ROOT}" "${DOTNET_TOOLS}"
    log "downloading dotnet-install.sh"
    curl -fsSL https://dot.net/v1/dotnet-install.sh -o /tmp/dotnet-install.sh
    chmod +x /tmp/dotnet-install.sh
    log "installing .NET SDK channel ${SDK_VERSION} into ${DOTNET_ROOT}"
    /tmp/dotnet-install.sh --channel "${SDK_VERSION}" \
        --install-dir "${DOTNET_ROOT}" \
        --no-path
    rm -f /tmp/dotnet-install.sh
    chmod -R a+rX "${DOTNET_ROOT}"
}

install_tools() {
    log "installing csharp-ls and scip-dotnet"
    "${DOTNET_ROOT}/dotnet" tool install --tool-path "${DOTNET_TOOLS}" csharp-ls \
        || warn "csharp-ls install failed"
    "${DOTNET_ROOT}/dotnet" tool install --tool-path "${DOTNET_TOOLS}" scip-dotnet \
        || warn "scip-dotnet install failed"
    chmod -R a+rX "${DOTNET_TOOLS}"
}

persist_profile() {
    mkdir -p /etc/profile.d
    cat > /etc/profile.d/djinn-dotnet.sh <<EOF
export DOTNET_ROOT="${DOTNET_ROOT}"
export DOTNET_CLI_TELEMETRY_OPTOUT=1
export PATH="${DOTNET_ROOT}:${DOTNET_TOOLS}:\$PATH"
EOF
    chmod 0644 /etc/profile.d/djinn-dotnet.sh
}

main() {
    install_os_deps
    install_dotnet_sdk
    install_tools
    persist_profile
    log "done"
}

main "$@"
