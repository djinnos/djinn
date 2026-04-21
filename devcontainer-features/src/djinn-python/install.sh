#!/usr/bin/env bash
# djinn-python install.sh
#
# Installs Python (via uv if chosen PM, else via pyenv), pyright (npm global),
# scip-python (pip), and the selected package manager.

set -euo pipefail

VERSION="${VERSION:-3.14}"
PM="${PM:-uv}"
INSTALL_DIR="/opt/djinn-python"

log() { printf '[djinn-python] %s\n' "$*"; }
warn() { printf '[djinn-python][WARN] %s\n' "$*" >&2; }

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
                ca-certificates curl git build-essential \
                libssl-dev zlib1g-dev libbz2-dev libreadline-dev \
                libsqlite3-dev libffi-dev liblzma-dev \
                xz-utils
            rm -rf /var/lib/apt/lists/*
            ;;
        apk)
            apk add --no-cache ca-certificates curl git build-base \
                openssl-dev zlib-dev bzip2-dev readline-dev \
                sqlite-dev libffi-dev xz-dev
            ;;
        dnf)
            dnf install -y ca-certificates curl git gcc make \
                openssl-devel zlib-devel bzip2-devel readline-devel \
                sqlite-devel libffi-devel xz-devel
            dnf clean all
            ;;
        yum)
            yum install -y ca-certificates curl git gcc make \
                openssl-devel zlib-devel bzip2-devel readline-devel \
                sqlite-devel libffi-devel xz-devel
            yum clean all
            ;;
        *)
            warn "unknown package manager; assuming build toolchain already present"
            ;;
    esac
}

install_uv() {
    if ! command -v uv >/dev/null 2>&1; then
        log "installing uv"
        curl -LsSf https://astral.sh/uv/install.sh | \
            UV_INSTALL_DIR=/usr/local/bin UV_UNMANAGED_INSTALL=1 sh
    fi
}

install_python_via_uv() {
    install_uv
    mkdir -p "${INSTALL_DIR}"
    export UV_PYTHON_INSTALL_DIR="${INSTALL_DIR}"
    log "installing python ${VERSION} via uv"
    uv python install "${VERSION}"
    # Symlink the chosen python into /opt/djinn-python/bin so PATH resolves it.
    local py_bin
    py_bin="$(uv python find "${VERSION}")"
    mkdir -p "${INSTALL_DIR}/bin"
    ln -sfn "${py_bin}" "${INSTALL_DIR}/bin/python3"
    ln -sfn "${py_bin}" "${INSTALL_DIR}/bin/python"
    # Ensure pip is available for scip-python install.
    "${py_bin}" -m ensurepip --upgrade || true
    "${py_bin}" -m pip install --upgrade pip
}

install_python_via_pyenv() {
    if [[ ! -d /opt/pyenv ]]; then
        log "installing pyenv"
        git clone --depth=1 https://github.com/pyenv/pyenv.git /opt/pyenv
    fi
    export PYENV_ROOT=/opt/pyenv
    export PATH="${PYENV_ROOT}/bin:${PATH}"
    log "installing python ${VERSION} via pyenv"
    pyenv install -s "${VERSION}"
    pyenv global "${VERSION}"
    mkdir -p "${INSTALL_DIR}/bin"
    ln -sfn "${PYENV_ROOT}/versions/${VERSION}/bin/python" "${INSTALL_DIR}/bin/python"
    ln -sfn "${PYENV_ROOT}/versions/${VERSION}/bin/python" "${INSTALL_DIR}/bin/python3"
    ln -sfn "${PYENV_ROOT}/versions/${VERSION}/bin/pip" "${INSTALL_DIR}/bin/pip"
}

install_pyright() {
    # Pyright is a Node-based LSP. Re-use whatever node is on PATH
    # (djinn-typescript Feature brings one, otherwise fall back to OS).
    if command -v npm >/dev/null 2>&1; then
        log "installing pyright (npm global)"
        npm install -g --silent pyright
    else
        warn "npm not on PATH; skipping pyright install"
        warn "add ghcr.io/djinnos/djinn-typescript or install node yourself"
    fi
}

install_scip_python() {
    log "installing scip-python"
    # scip-python is published on npm by Sourcegraph, not pip.
    if command -v npm >/dev/null 2>&1; then
        npm install -g --silent @sourcegraph/scip-python
    else
        warn "npm not on PATH; skipping scip-python install"
    fi
}

install_pm() {
    case "${PM}" in
        uv)
            install_uv
            ;;
        poetry)
            "${INSTALL_DIR}/bin/pip" install --no-cache-dir poetry
            ;;
        pdm)
            "${INSTALL_DIR}/bin/pip" install --no-cache-dir pdm
            ;;
        pip)
            log "pip bundled with python; no extra install"
            ;;
        *)
            warn "unknown PM ${PM}; skipping PM install"
            ;;
    esac
}

persist_profile() {
    mkdir -p /etc/profile.d
    cat > /etc/profile.d/djinn-python.sh <<EOF
export PATH="${INSTALL_DIR}/bin:\$PATH"
EOF
    chmod 0644 /etc/profile.d/djinn-python.sh
}

main() {
    install_os_deps
    if [[ "${PM}" == "uv" ]]; then
        install_python_via_uv
    else
        install_python_via_pyenv
    fi
    install_pyright
    install_scip_python
    install_pm
    persist_profile
    log "done"
}

main "$@"
