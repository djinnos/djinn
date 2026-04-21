#!/usr/bin/env bash
# djinn-java install.sh
#
# Installs SDKMAN, JDK, build tool (gradle/maven), jdtls, and scip-java.

set -euo pipefail

JDK_VERSION="${JDK_VERSION:-25-tem}"
BUILD_TOOL="${BUILD_TOOL:-gradle}"
export SDKMAN_DIR="${SDKMAN_DIR:-/usr/local/sdkman}"
INSTALL_DIR="/opt/djinn-java"
JDTLS_VERSION="1.40.0"

log() { printf '[djinn-java] %s\n' "$*"; }
warn() { printf '[djinn-java][WARN] %s\n' "$*" >&2; }

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
                ca-certificates curl git zip unzip bash
            rm -rf /var/lib/apt/lists/*
            ;;
        apk)
            apk add --no-cache ca-certificates curl git zip unzip bash
            ;;
        dnf)
            dnf install -y ca-certificates curl git zip unzip bash
            dnf clean all
            ;;
        yum)
            yum install -y ca-certificates curl git zip unzip bash
            yum clean all
            ;;
        *)
            warn "unknown package manager; assuming unzip/curl present"
            ;;
    esac
}

install_sdkman() {
    if [[ ! -s "${SDKMAN_DIR}/bin/sdkman-init.sh" ]]; then
        log "installing SDKMAN into ${SDKMAN_DIR}"
        export SDKMAN_DIR
        curl -fsSL "https://get.sdkman.io?rcupdate=false" | bash
    fi
    # shellcheck disable=SC1091
    source "${SDKMAN_DIR}/bin/sdkman-init.sh"
}

install_jdk_and_build_tool() {
    # shellcheck disable=SC1091
    source "${SDKMAN_DIR}/bin/sdkman-init.sh"
    log "installing JDK ${JDK_VERSION}"
    sdk install java "${JDK_VERSION}" < /dev/null
    sdk default java "${JDK_VERSION}" < /dev/null
    case "${BUILD_TOOL}" in
        gradle)
            log "installing gradle"
            sdk install gradle < /dev/null
            ;;
        maven)
            log "installing maven"
            sdk install maven < /dev/null
            ;;
        *)
            warn "unknown build tool ${BUILD_TOOL}; skipping"
            ;;
    esac
    chmod -R a+rX "${SDKMAN_DIR}"
}

install_jdtls() {
    log "installing jdtls ${JDTLS_VERSION}"
    mkdir -p "${INSTALL_DIR}/jdtls"
    local url="https://download.eclipse.org/jdtls/milestones/${JDTLS_VERSION}/jdt-language-server-${JDTLS_VERSION}.tar.gz"
    if ! curl -fsSL "${url}" -o /tmp/jdtls.tgz; then
        warn "failed to download jdtls ${JDTLS_VERSION}; skipping"
        return 0
    fi
    tar -xzf /tmp/jdtls.tgz -C "${INSTALL_DIR}/jdtls"
    rm -f /tmp/jdtls.tgz
    mkdir -p "${INSTALL_DIR}/bin"
    cat > "${INSTALL_DIR}/bin/jdtls" <<EOF
#!/usr/bin/env bash
exec "\${JAVA_HOME:-/usr/local/sdkman/candidates/java/current}/bin/java" \\
    -Declipse.application=org.eclipse.jdt.ls.core.id1 \\
    -Dosgi.bundles.defaultStartLevel=4 \\
    -Declipse.product=org.eclipse.jdt.ls.core.product \\
    -jar "${INSTALL_DIR}/jdtls/plugins/org.eclipse.equinox.launcher_*.jar" \\
    -configuration "${INSTALL_DIR}/jdtls/config_linux" \\
    "\$@"
EOF
    chmod +x "${INSTALL_DIR}/bin/jdtls"
}

install_scip_java() {
    # scip-java is invoked via the coursier bootstrap script and builds via gradle/maven.
    log "installing scip-java (coursier bootstrap)"
    mkdir -p "${INSTALL_DIR}/bin"
    curl -fsSL https://raw.githubusercontent.com/sourcegraph/scip-java/main/scip-java.sh \
        -o "${INSTALL_DIR}/bin/scip-java" || {
            warn "scip-java bootstrap download failed; skipping"
            return 0
        }
    chmod +x "${INSTALL_DIR}/bin/scip-java"
}

persist_profile() {
    mkdir -p /etc/profile.d
    cat > /etc/profile.d/djinn-java.sh <<EOF
export SDKMAN_DIR="${SDKMAN_DIR}"
[[ -s "\${SDKMAN_DIR}/bin/sdkman-init.sh" ]] && source "\${SDKMAN_DIR}/bin/sdkman-init.sh"
export PATH="${INSTALL_DIR}/bin:\$PATH"
EOF
    chmod 0644 /etc/profile.d/djinn-java.sh
}

main() {
    install_os_deps
    install_sdkman
    install_jdk_and_build_tool
    install_jdtls
    install_scip_java
    persist_profile
    log "done"
}

main "$@"
