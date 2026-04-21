#!/usr/bin/env bash
# djinn-agent-worker install.sh
#
# Copies the musl-static djinn-agent-worker binary (packaged alongside this
# script by CI) into /opt/djinn/bin and ensures a minimal set of base tools is
# present (git, ca-certificates, bash, tini). Runs as root during image build.

set -euo pipefail

INSTALL_DIR="/opt/djinn"
BIN_DIR="${INSTALL_DIR}/bin"
SRC_DIR="$(cd "$(dirname "$0")" && pwd)"

log() { printf '[djinn-agent-worker] %s\n' "$*"; }
warn() { printf '[djinn-agent-worker][WARN] %s\n' "$*" >&2; }

detect_pkg_mgr() {
    if command -v apt-get >/dev/null 2>&1; then
        echo "apt"
    elif command -v apk >/dev/null 2>&1; then
        echo "apk"
    elif command -v dnf >/dev/null 2>&1; then
        echo "dnf"
    elif command -v yum >/dev/null 2>&1; then
        echo "yum"
    else
        echo "unknown"
    fi
}

install_base_tools() {
    local pm
    pm="$(detect_pkg_mgr)"
    local pkgs=(git ca-certificates bash tini)

    case "$pm" in
        apt)
            export DEBIAN_FRONTEND=noninteractive
            apt-get update -y
            apt-get install -y --no-install-recommends "${pkgs[@]}"
            rm -rf /var/lib/apt/lists/*
            ;;
        apk)
            apk add --no-cache "${pkgs[@]}"
            ;;
        dnf)
            dnf install -y "${pkgs[@]}"
            dnf clean all
            ;;
        yum)
            yum install -y "${pkgs[@]}"
            yum clean all
            ;;
        *)
            warn "unknown package manager; skipping base-tool install"
            ;;
    esac
}

install_worker_binary() {
    mkdir -p "${BIN_DIR}"
    local src_bin="${SRC_DIR}/bin/djinn-agent-worker"
    if [[ ! -f "${src_bin}" ]]; then
        warn "worker binary not packaged at ${src_bin}."
        warn "CI populates bin/djinn-agent-worker before feature publish."
        warn "Skipping binary install — image will be incomplete."
        return 0
    fi
    install -m 0755 "${src_bin}" "${BIN_DIR}/djinn-agent-worker"
    log "installed worker binary to ${BIN_DIR}/djinn-agent-worker"
}

persist_path() {
    mkdir -p /etc/profile.d
    cat > /etc/profile.d/djinn.sh <<'EOF'
export PATH="/opt/djinn/bin:${PATH}"
EOF
    chmod 0644 /etc/profile.d/djinn.sh
}

main() {
    log "installing djinn-agent-worker into ${INSTALL_DIR}"
    install_base_tools
    install_worker_binary
    persist_path
    log "done"
}

main "$@"
