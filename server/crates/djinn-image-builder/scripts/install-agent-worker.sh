#!/usr/bin/env bash
# Final-layer step: the djinn-agent-worker binary has already been
# `COPY`'d from the `djinn/agent-worker:<sha>` helper image into
# /opt/djinn/bin/djinn-agent-worker by the Dockerfile. This script
# just drops the PATH fragment so every shell picks it up at login.
#
# The fragment name is `00-djinn.sh` — the leading `00-` guarantees
# it loads before every language fragment (10-rust.sh, 20-node.sh, ...)
# so tools in /opt/djinn/bin aren't shadowed by user-controlled PATH
# entries inside rustup/fnm/uv.
set -euo pipefail

if [ ! -x /opt/djinn/bin/djinn-agent-worker ]; then
    echo "[install-agent-worker] /opt/djinn/bin/djinn-agent-worker missing or not executable" >&2
    exit 1
fi

cat > /etc/profile.d/00-djinn.sh <<'EOF'
export PATH="/opt/djinn/bin:${PATH}"
EOF
chmod 0644 /etc/profile.d/00-djinn.sh
