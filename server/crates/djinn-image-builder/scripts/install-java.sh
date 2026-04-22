#!/usr/bin/env bash
# Install the requested JDK version via Eclipse Temurin tarball.
#
# Inputs (env):
#   JAVA_VERSION — required. e.g. "21", "17".
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
