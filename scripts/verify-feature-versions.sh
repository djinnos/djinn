#!/usr/bin/env bash
# verify-feature-versions.sh
#
# Compare the `default` values in devcontainer-features/src/*/devcontainer-feature.json
# against upstream release APIs. Logs warnings when drift is detected; never
# exits non-zero. Intended to run in CI as a soft check.

set -uo pipefail

FEATURES_DIR="${FEATURES_DIR:-devcontainer-features/src}"

log() { printf '[verify] %s\n' "$*"; }
warn() { printf '::warning::[verify] %s\n' "$*" >&2; }
drift() { printf '::warning::[drift] %s\n' "$*" >&2; }

require_jq() {
    if ! command -v jq >/dev/null 2>&1; then
        warn "jq missing; skipping all verification"
        exit 0
    fi
}

# Extract a scalar default for a Feature option.
# Usage: default_of <feature-id> <option-name>
default_of() {
    local fid="$1"
    local opt="$2"
    local file="${FEATURES_DIR}/${fid}/devcontainer-feature.json"
    [[ -f "$file" ]] || return 0
    jq -r ".options.\"${opt}\".default // empty" "$file"
}

check_node() {
    local default
    default="$(default_of djinn-typescript node_version)"
    [[ -n "$default" ]] || return 0
    log "djinn-typescript node_version default = ${default}"
    local latest_lts
    latest_lts="$(curl -fsSL https://nodejs.org/dist/index.json \
        | jq -r '[.[] | select(.lts != false)] | .[0].version' 2>/dev/null || true)"
    if [[ -n "$latest_lts" ]]; then
        log "upstream node LTS = ${latest_lts}"
        # major-version comparison
        local latest_major
        latest_major="$(echo "$latest_lts" | sed 's/^v//;s/\..*//')"
        if [[ "$default" != "$latest_major" && "$default" != "${latest_lts#v}"* ]]; then
            drift "djinn-typescript default node_version=${default} vs upstream LTS=${latest_lts}"
        fi
    fi
}

check_python() {
    local default
    default="$(default_of djinn-python version)"
    [[ -n "$default" ]] || return 0
    log "djinn-python version default = ${default}"
    # python.org JSON index
    local latest
    latest="$(curl -fsSL https://www.python.org/api/v2/downloads/release/?is_published=true \
        | jq -r 'sort_by(.release_date) | reverse
                 | map(select(.pre_release == false))
                 | .[0].name' 2>/dev/null || true)"
    if [[ -n "$latest" ]]; then
        log "upstream python latest = ${latest}"
        if [[ "$latest" != *"$default"* ]]; then
            drift "djinn-python default version=${default} vs upstream=${latest}"
        fi
    fi
}

check_go() {
    local default
    default="$(default_of djinn-go version)"
    [[ -n "$default" ]] || return 0
    log "djinn-go version default = ${default}"
    local latest
    latest="$(curl -fsSL 'https://go.dev/dl/?mode=json' \
        | jq -r '.[0].version' 2>/dev/null | sed 's/^go//' || true)"
    if [[ -n "$latest" ]]; then
        log "upstream go latest = ${latest}"
        if [[ "$default" != "$latest" ]]; then
            drift "djinn-go default version=${default} vs upstream=${latest}"
        fi
    fi
}

check_dotnet() {
    local default
    default="$(default_of djinn-dotnet sdk_version)"
    [[ -n "$default" ]] || return 0
    log "djinn-dotnet sdk_version default = ${default}"
    # endoflife.date gives a stable signal
    local latest
    latest="$(curl -fsSL https://endoflife.date/api/dotnet.json \
        | jq -r 'map(select(.lts == true)) | .[0].latest' 2>/dev/null || true)"
    if [[ -n "$latest" ]]; then
        log "upstream .NET LTS latest = ${latest}"
        if [[ "$latest" != "$default"* ]]; then
            drift "djinn-dotnet default sdk_version=${default} vs upstream LTS=${latest}"
        fi
    fi
}

check_ruby() {
    local default
    default="$(default_of djinn-ruby version)"
    [[ -n "$default" ]] || return 0
    log "djinn-ruby version default = ${default}"
    local latest
    latest="$(curl -fsSL https://endoflife.date/api/ruby.json \
        | jq -r 'map(select(.eol == false or (.eol | type == "string" and . > now | todate))) | .[0].latest' 2>/dev/null || true)"
    if [[ -n "$latest" ]]; then
        log "upstream ruby latest supported = ${latest}"
    fi
}

check_java() {
    local default
    default="$(default_of djinn-java jdk_version)"
    [[ -n "$default" ]] || return 0
    log "djinn-java jdk_version default = ${default}"
    local latest_lts
    latest_lts="$(curl -fsSL https://endoflife.date/api/java.json \
        | jq -r 'map(select(.lts == true)) | .[0].latest' 2>/dev/null || true)"
    if [[ -n "$latest_lts" ]]; then
        log "upstream Java LTS latest = ${latest_lts}"
    fi
}

check_clang() {
    local default
    default="$(default_of djinn-clang version)"
    [[ -n "$default" ]] || return 0
    log "djinn-clang version default = ${default}"
    # LLVM releases API
    local latest
    latest="$(curl -fsSL https://api.github.com/repos/llvm/llvm-project/releases \
        | jq -r '.[] | select(.prerelease == false) | .tag_name' 2>/dev/null \
        | head -1 | sed 's/^llvmorg-//;s/\..*//' || true)"
    if [[ -n "$latest" ]]; then
        log "upstream LLVM major = ${latest}"
        if [[ "$default" != "$latest" ]]; then
            drift "djinn-clang default version=${default} vs upstream LLVM major=${latest}"
        fi
    fi
}

main() {
    require_jq
    log "verifying feature-default versions against upstream"
    check_node
    check_python
    check_go
    check_dotnet
    check_ruby
    check_java
    check_clang
    log "done"
}

main "$@"
