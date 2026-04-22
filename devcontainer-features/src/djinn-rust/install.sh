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
    # We install rust-analyzer two ways, and they matter for different
    # callers:
    #
    # 1. As a rustup component on ${TOOLCHAIN} (the feature's configured
    #    toolchain). This is what developers expect when they run
    #    `rustup which rust-analyzer` or use the feature inside an
    #    interactive IDE session. Degrade gracefully — very old nightlies
    #    don't carry the component.
    log "installing rust-analyzer component for ${TOOLCHAIN}"
    if ! "${CARGO_HOME}/bin/rustup" component add rust-analyzer --toolchain "${TOOLCHAIN}"; then
        warn "rust-analyzer component unavailable for ${TOOLCHAIN}; skipping"
    fi

    # 2. As a standalone binary at ${CARGO_HOME}/bin/rust-analyzer
    #    overwriting the rustup proxy shim. This is the load-bearing
    #    install for djinn's canonical-graph warm pipeline.
    #
    #    Why: the proxy shim consults ${PROJECT}/rust-toolchain.toml at
    #    invocation time and redirects to that toolchain's
    #    rust-analyzer. When a project pins `channel = "1.94.1"` (or any
    #    specific version the feature didn't install rust-analyzer for),
    #    the shim errors with `Unknown binary 'rust-analyzer' in
    #    official toolchain '1.94.1-...'` — even though rust-analyzer is
    #    sitting right there under the feature-installed toolchain. The
    #    standalone binary doesn't participate in toolchain resolution;
    #    SCIP emission works regardless of project pin.
    #
    #    cargo/rustc/clippy proxies are left alone — they _need_ to
    #    respect the project's pin. Only rust-analyzer is special-cased.
    install_standalone_rust_analyzer
}

install_standalone_rust_analyzer() {
    local arch url tmp
    case "$(uname -m)" in
        x86_64)  arch="x86_64-unknown-linux-gnu" ;;
        aarch64) arch="aarch64-unknown-linux-gnu" ;;
        *) warn "unsupported arch $(uname -m) for standalone rust-analyzer; skipping"; return 0 ;;
    esac
    # Pin a known-good release instead of following `latest` so image
    # content is reproducible build-to-build.
    local ra_release="2026-03-31"
    url="https://github.com/rust-lang/rust-analyzer/releases/download/${ra_release}/rust-analyzer-${arch}.gz"
    log "downloading standalone rust-analyzer ${ra_release} for ${arch}"
    tmp="$(mktemp)"
    if ! curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "${tmp}.gz"; then
        warn "standalone rust-analyzer download failed; falling back to rustup proxy only"
        rm -f "${tmp}.gz"
        return 0
    fi
    gunzip "${tmp}.gz"  # produces $tmp
    # Overwrite the rustup proxy shim at ${CARGO_HOME}/bin/rust-analyzer
    # so `rust-analyzer` on PATH hits the standalone. cargo/rustc/clippy
    # shims are untouched.
    install -m 0755 "$tmp" "${CARGO_HOME}/bin/rust-analyzer"
    rm -f "$tmp"
    log "standalone rust-analyzer installed at ${CARGO_HOME}/bin/rust-analyzer"
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
