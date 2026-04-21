#!/usr/bin/env bash
# djinn-rust install.sh
#
# Installs rustup + the requested toolchain + rust-analyzer component.
# rust-analyzer supports SCIP emission natively (`rust-analyzer scip`).

set -euo pipefail

TOOLCHAIN="${TOOLCHAIN:-stable}"
export RUSTUP_HOME="${RUSTUP_HOME:-/usr/local/rustup}"
export CARGO_HOME="${CARGO_HOME:-/usr/local/cargo}"

log() { printf '[djinn-rust] %s\n' "$*"; }
warn() { printf '[djinn-rust][WARN] %s\n' "$*" >&2; }

detect_pkg_mgr() {
    if command -v apt-get >/dev/null 2>&1; then echo apt
    elif command -v apk >/dev/null 2>&1; then echo apk
    elif command -v dnf >/dev/null 2>&1; then echo dnf
    elif command -v yum >/dev/null 2>&1; then echo yum
    else echo unknown
    fi
}

install_build_deps() {
    local pm
    pm="$(detect_pkg_mgr)"
    case "$pm" in
        apt)
            export DEBIAN_FRONTEND=noninteractive
            apt-get update -y
            apt-get install -y --no-install-recommends \
                ca-certificates curl gcc libc6-dev pkg-config
            rm -rf /var/lib/apt/lists/*
            ;;
        apk)
            apk add --no-cache ca-certificates curl gcc musl-dev pkgconfig
            ;;
        dnf)
            dnf install -y ca-certificates curl gcc glibc-devel pkgconf-pkg-config
            dnf clean all
            ;;
        yum)
            yum install -y ca-certificates curl gcc glibc-devel pkgconfig
            yum clean all
            ;;
        *)
            warn "unknown package manager; assuming build toolchain already present"
            ;;
    esac
}

install_rustup() {
    mkdir -p "${RUSTUP_HOME}" "${CARGO_HOME}"
    if ! command -v rustup >/dev/null 2>&1; then
        log "installing rustup (toolchain=${TOOLCHAIN})"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --profile minimal --default-toolchain "${TOOLCHAIN}" --no-modify-path
    else
        log "rustup already present, ensuring toolchain ${TOOLCHAIN}"
        rustup toolchain install "${TOOLCHAIN}"
        rustup default "${TOOLCHAIN}"
    fi
    chmod -R a+rX "${RUSTUP_HOME}" "${CARGO_HOME}"
}

install_rust_analyzer() {
    log "installing rust-analyzer component for ${TOOLCHAIN}"
    # Some toolchains (very old nightlies) don't carry the component; degrade gracefully.
    if ! "${CARGO_HOME}/bin/rustup" component add rust-analyzer --toolchain "${TOOLCHAIN}"; then
        warn "rust-analyzer component unavailable for ${TOOLCHAIN}; skipping"
    fi
}

persist_profile() {
    mkdir -p /etc/profile.d
    cat > /etc/profile.d/djinn-rust.sh <<EOF
export RUSTUP_HOME="${RUSTUP_HOME}"
export CARGO_HOME="${CARGO_HOME}"
export PATH="${CARGO_HOME}/bin:\$PATH"
EOF
    chmod 0644 /etc/profile.d/djinn-rust.sh
}

main() {
    install_build_deps
    install_rustup
    install_rust_analyzer
    persist_profile
    log "done"
}

main "$@"
