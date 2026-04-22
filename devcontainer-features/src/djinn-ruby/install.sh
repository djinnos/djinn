#!/usr/bin/env bash
# djinn-ruby install.sh
#
# Installs rbenv + ruby-build, the requested Ruby version, ruby-lsp, and scip-ruby.

set -euo pipefail

VERSION="${VERSION:-3.4}"
export RBENV_ROOT="${RBENV_ROOT:-/usr/local/rbenv}"
export PATH="${RBENV_ROOT}/shims:${RBENV_ROOT}/bin:${PATH}"

log() { printf '[djinn-ruby] %s\n' "$*"; }
warn() { printf '[djinn-ruby][WARN] %s\n' "$*" >&2; }

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
                libssl-dev libreadline-dev zlib1g-dev libyaml-dev \
                libffi-dev libgmp-dev libncurses5-dev autoconf bison
            rm -rf /var/lib/apt/lists/*
            ;;
        apk)
            apk add --no-cache ca-certificates curl git build-base \
                openssl-dev readline-dev zlib-dev yaml-dev libffi-dev \
                gmp-dev ncurses-dev autoconf bison
            ;;
        dnf)
            dnf install -y ca-certificates curl git gcc make \
                openssl-devel readline-devel zlib-devel libyaml-devel \
                libffi-devel gmp-devel ncurses-devel autoconf bison
            dnf clean all
            ;;
        yum)
            yum install -y ca-certificates curl git gcc make \
                openssl-devel readline-devel zlib-devel libyaml-devel \
                libffi-devel gmp-devel ncurses-devel autoconf bison
            yum clean all
            ;;
        *)
            warn "unknown package manager; assuming build toolchain present"
            ;;
    esac
}

install_rbenv() {
    if [[ ! -d "${RBENV_ROOT}" ]]; then
        log "installing rbenv"
        git clone --depth=1 https://github.com/rbenv/rbenv.git "${RBENV_ROOT}"
    fi
    if [[ ! -d "${RBENV_ROOT}/plugins/ruby-build" ]]; then
        git clone --depth=1 https://github.com/rbenv/ruby-build.git \
            "${RBENV_ROOT}/plugins/ruby-build"
    fi
}

install_ruby() {
    # ruby-build definitions are patch-level (`3.4.4`, `3.3.7`…); a
    # bare `3.4` is not a valid definition and errors with "definition
    # not found". Resolve major.minor to the latest known patch if the
    # caller passed a short form.
    local rb="${RBENV_ROOT}/plugins/ruby-build/bin/ruby-build"
    local resolved="${VERSION}"
    if ! "$rb" --definitions | grep -qxF "${VERSION}"; then
        resolved="$("$rb" --definitions | grep -E "^${VERSION//./\\.}\.[0-9]+$" | sort -V | tail -1)"
        if [[ -z "$resolved" ]]; then
            warn "ruby-build has no definition matching '${VERSION}'; aborting"
            return 1
        fi
        log "resolved ruby ${VERSION} to ${resolved}"
    fi
    log "installing ruby ${resolved}"
    "${RBENV_ROOT}/bin/rbenv" install -s "${resolved}"
    "${RBENV_ROOT}/bin/rbenv" global "${resolved}"
    "${RBENV_ROOT}/shims/gem" update --system --no-document || true
    chmod -R a+rX "${RBENV_ROOT}"
}

install_gems() {
    log "installing ruby-lsp and scip-ruby"
    "${RBENV_ROOT}/shims/gem" install --no-document ruby-lsp || \
        warn "ruby-lsp install failed"
    "${RBENV_ROOT}/shims/gem" install --no-document scip-ruby || \
        warn "scip-ruby install failed (may be unavailable on this arch)"
    "${RBENV_ROOT}/bin/rbenv" rehash
}

persist_profile() {
    mkdir -p /etc/profile.d
    cat > /etc/profile.d/djinn-ruby.sh <<EOF
export RBENV_ROOT="${RBENV_ROOT}"
export PATH="\${RBENV_ROOT}/shims:\${RBENV_ROOT}/bin:\$PATH"
EOF
    chmod 0644 /etc/profile.d/djinn-ruby.sh
}

main() {
    install_os_deps
    install_rbenv
    install_ruby
    install_gems
    persist_profile
    log "done"
}

main "$@"
