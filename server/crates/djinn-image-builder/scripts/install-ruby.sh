#!/usr/bin/env bash
# Install the requested Ruby version via rbenv + ruby-build. Skeleton
# — deliberate minimum until a Ruby project actually exercises this.
#
# Inputs (env):
#   RUBY_VERSION — required. e.g. "3.3.0".
set -euo pipefail

: "${RUBY_VERSION:?RUBY_VERSION is required, e.g. \"3.3.0\"}"

export RBENV_ROOT="${RBENV_ROOT:-/usr/local/rbenv}"
git clone --depth 1 https://github.com/rbenv/rbenv.git "${RBENV_ROOT}"
git clone --depth 1 https://github.com/rbenv/ruby-build.git "${RBENV_ROOT}/plugins/ruby-build"

export PATH="${RBENV_ROOT}/bin:${RBENV_ROOT}/shims:${PATH}"
eval "$(rbenv init -)"
rbenv install "${RUBY_VERSION}"
rbenv global "${RUBY_VERSION}"

cat > /etc/profile.d/60-ruby.sh <<'EOF'
export RBENV_ROOT=/usr/local/rbenv
export PATH="${RBENV_ROOT}/bin:${RBENV_ROOT}/shims:${PATH}"
if command -v rbenv >/dev/null 2>&1; then
    eval "$(rbenv init -)"
fi
EOF
chmod 0644 /etc/profile.d/60-ruby.sh
