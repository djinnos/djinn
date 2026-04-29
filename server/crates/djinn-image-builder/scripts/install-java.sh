#!/usr/bin/env bash
# Install the requested JDK version via Eclipse Temurin tarball.
#
# Inputs (env):
#   JAVA_VERSION       — required. e.g. "21", "17".
#   SCIP_INDEXER       — optional. "scip-java" → installs the indexer at
#                        `${SCIP_JAVA_VERSION}` (default `latest`) from
#                        the official GitHub release.
#   SCIP_JAVA_VERSION  — optional. Pin scip-java to a specific tag, e.g.
#                        `v0.12.3`. The release artifact is a single
#                        Coursier-generated launcher (~129 MB).
set -euo pipefail

: "${JAVA_VERSION:?JAVA_VERSION is required, e.g. \"21\"}"

arch="$(uname -m)"
case "${arch}" in
    x86_64)  jarch="x64" ;;
    aarch64) jarch="aarch64" ;;
    *) echo "[install-java] unsupported arch ${arch}" >&2; exit 1 ;;
esac

# Latest within major via Adoptium's API — reproducibility is via the
# image hash (any new JDK release bumps script output). For stricter
# pinning, the env config can set JAVA_VERSION to a full "21.0.2+13"
# string and we'll pass it through.
jdk_dir="/usr/local/jdk-${JAVA_VERSION}"
mkdir -p "${jdk_dir}"
url="https://api.adoptium.net/v3/binary/latest/${JAVA_VERSION}/ga/linux/${jarch}/jdk/hotspot/normal/eclipse"
curl --proto '=https' --tlsv1.2 -fsSL "${url}" | tar -C "${jdk_dir}" --strip-components=1 -xzf -

cat > /etc/profile.d/50-java.sh <<EOF
export JAVA_HOME=${jdk_dir}
export PATH="\${JAVA_HOME}/bin:\${PATH}"
EOF
chmod 0644 /etc/profile.d/50-java.sh

if [ "${SCIP_INDEXER:-}" = "scip-java" ]; then
    sj_version="${SCIP_JAVA_VERSION:-latest}"
    if [ "${sj_version}" = "latest" ]; then
        sj_version="$(curl -fsSL https://api.github.com/repos/sourcegraph/scip-java/releases/latest \
            | grep -oE '"tag_name"[[:space:]]*:[[:space:]]*"v[0-9.]+"' | head -n1 \
            | sed -E 's/.*"(v[0-9.]+)"/\1/')"
        [ -n "${sj_version}" ] || { echo "[install-java] could not resolve scip-java latest" >&2; exit 1; }
    fi
    sj_url="https://github.com/sourcegraph/scip-java/releases/download/${sj_version}/scip-java-${sj_version}"
    curl --proto '=https' --tlsv1.2 -fsSL "${sj_url}" -o /usr/local/bin/scip-java
    chmod +x /usr/local/bin/scip-java
fi
