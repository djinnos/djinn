#!/usr/bin/env bash
# djinn-typescript install.sh
#
# Installs Node.js via nvm, typescript-language-server + TypeScript (npm global),
# scip-typescript (npm global), and the selected package manager.

# NOTE: `-u` (nounset) is intentionally not enabled — nvm.sh dereferences
# several of its own internal vars before defining them, and we source
# it as part of normal install flow.
set -eo pipefail

NODE_VERSION="${NODE_VERSION:-24}"
PM="${PM:-pnpm}"
export NVM_DIR="${NVM_DIR:-/usr/local/nvm}"

log() { printf '[djinn-typescript] %s\n' "$*"; }
warn() { printf '[djinn-typescript][WARN] %s\n' "$*" >&2; }

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
            apt-get install -y --no-install-recommends ca-certificates curl git bash
            rm -rf /var/lib/apt/lists/*
            ;;
        apk)
            apk add --no-cache ca-certificates curl git bash
            ;;
        dnf)
            dnf install -y ca-certificates curl git bash
            dnf clean all
            ;;
        yum)
            yum install -y ca-certificates curl git bash
            yum clean all
            ;;
        *)
            warn "unknown package manager; assuming curl/git already present"
            ;;
    esac
}

install_nvm_and_node() {
    mkdir -p "${NVM_DIR}"
    if [[ ! -s "${NVM_DIR}/nvm.sh" ]]; then
        log "installing nvm into ${NVM_DIR}"
        curl -fsSL https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh \
            | PROFILE=/dev/null bash
    fi
    # nvm install scripts read/write $HOME; point it at a stable location.
    export HOME="${HOME:-/root}"
    # shellcheck disable=SC1091
    . "${NVM_DIR}/nvm.sh"
    log "installing node ${NODE_VERSION} via nvm"
    nvm install "${NODE_VERSION}"
    nvm alias default "${NODE_VERSION}"
    local resolved
    resolved="$(nvm version "${NODE_VERSION}")"
    ln -sfn "${NVM_DIR}/versions/node/${resolved}" "${NVM_DIR}/current"
    chmod -R a+rX "${NVM_DIR}"
}

npm_global() {
    "${NVM_DIR}/current/bin/npm" install -g --silent "$@"
}

install_lsp_and_indexer() {
    log "installing typescript + typescript-language-server + scip-typescript"
    npm_global \
        typescript \
        typescript-language-server \
        @sourcegraph/scip-typescript
}

install_pm() {
    case "${PM}" in
        pnpm)
            log "installing pnpm via corepack"
            "${NVM_DIR}/current/bin/corepack" enable
            "${NVM_DIR}/current/bin/corepack" prepare pnpm@latest --activate
            ;;
        yarn)
            "${NVM_DIR}/current/bin/corepack" enable
            "${NVM_DIR}/current/bin/corepack" prepare yarn@stable --activate
            ;;
        bun)
            log "installing bun"
            curl -fsSL https://bun.sh/install | BUN_INSTALL=/opt/bun bash
            ln -sfn /opt/bun/bin/bun /usr/local/bin/bun || true
            ;;
        deno)
            log "installing deno"
            curl -fsSL https://deno.land/install.sh | DENO_INSTALL=/opt/deno sh
            ln -sfn /opt/deno/bin/deno /usr/local/bin/deno || true
            ;;
        npm)
            log "npm already provided by Node"
            ;;
        *)
            warn "unknown PM ${PM}; skipping PM install"
            ;;
    esac
}

persist_profile() {
    mkdir -p /etc/profile.d
    cat > /etc/profile.d/djinn-typescript.sh <<EOF
export NVM_DIR="${NVM_DIR}"
export PATH="${NVM_DIR}/current/bin:\$PATH"
EOF
    chmod 0644 /etc/profile.d/djinn-typescript.sh
}

main() {
    install_os_deps
    install_nvm_and_node
    install_lsp_and_indexer
    install_pm
    persist_profile
    log "done"
}

main "$@"
