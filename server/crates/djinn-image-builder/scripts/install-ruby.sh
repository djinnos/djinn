#!/usr/bin/env bash
# Install the requested Ruby version via rbenv + ruby-build. Skeleton
# — deliberate minimum until a Ruby project actually exercises this.
#
# Inputs (env):
#   RUBY_VERSION       — required. e.g. "3.3.0".
#   SCIP_INDEXER       — optional. "scip-ruby" → installs the indexer at
#                        `${SCIP_RUBY_VERSION}` (default `latest`) via
#                        RubyGems.
#   SCIP_RUBY_VERSION  — optional. Pin scip-ruby to a specific gem
#                        version. The upstream tag format is
#                        `scip-ruby-v0.4.7` but the gem uses the bare
#                        `0.4.7` — pass the bare number here.
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

if [ "${SCIP_INDEXER:-}" = "scip-ruby" ]; then
    if [ -n "${SCIP_RUBY_VERSION:-}" ] && [ "${SCIP_RUBY_VERSION}" != "latest" ]; then
        gem install scip-ruby -v "${SCIP_RUBY_VERSION}"
    else
        gem install scip-ruby
    fi
fi
