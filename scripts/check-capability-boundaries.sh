#!/bin/sh
# Shared capability-boundary guard plumbing.
#
# This script is the common entrypoint for repository-local capability-boundary
# checks.  Capability-specific scripts (git, http, k8s) delegate to it by
# passing the capability name, owner crate, remediation owner path, and a grep
# pattern via the environment variables listed below.
#
# Usage:
#   CAPABILITY=git \
#   OWNER=server/crates/djinn-git \
#   REMEDIATION=djinn-git \
#   PATTERN='(git2::|use git2|Command::new\("git"\)|...)' \
#   ./scripts/check-capability-boundaries.sh
#
#   BASE_SHA=<sha> ./scripts/check-capability-boundaries.sh
#   printf '%s\n' path/to/file.rs | ./scripts/check-capability-boundaries.sh --files-from-stdin
#   CAPABILITY_BOUNDARY_MODE=files-from-stdin ./scripts/check-capability-boundaries.sh < changed-files.txt
#
# Modes:
#   (default)       Diff BASE_SHA..HEAD to get changed Rust files, then check.
#   --files-from-stdin
#                   Read changed file paths from stdin; check only in-scope files.
#
# Environment:
#   CAPABILITY              Short name of the capability (git, http, k8s).
#   OWNER                   Crate directory that owns the capability
#                           (e.g. server/crates/djinn-git).
#   REMEDIATION             Owner path used in diagnostics
#                           (e.g. djinn-git, djinn-provider, djinn-k8s).
#   PATTERN                 POSIX extended grep pattern used to detect usage.
#   CAPABILITY_BOUNDARY_MODE
#                           Override mode selection: "diff" or "files-from-stdin".
#   BASE_SHA                Base commit to diff HEAD against (default: origin/main).
#   ALLOWLIST               Optional path to a TOML allowlist (default:
#                           scripts/capability-boundary-allowlist.toml).
#
# Exit codes:
#   0  No violations found (or no in-scope files to check).
#   1  Violations found — capability usage detected outside the owner crate.
#   2  Usage / configuration error.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

CAPABILITY=${CAPABILITY:-}
OWNER=${OWNER:-}
REMEDIATION=${REMEDIATION:-}
PATTERN=${PATTERN:-}
MODE=${CAPABILITY_BOUNDARY_MODE:-diff}
ALLOWLIST=${ALLOWLIST:-"$SCRIPT_DIR/capability-boundary-allowlist.toml"}

usage() {
    cat <<EOF
Usage: $0 [--diff|--files-from-stdin]

Shared capability-boundary guard for Rust sources.  Required environment:
  CAPABILITY  Short name of the capability (git, http, k8s).
  OWNER       Directory of the crate that owns the capability
              (e.g. server/crates/djinn-git).
  REMEDIATION Owner path used in diagnostics (e.g. djinn-git).
  PATTERN     POSIX extended grep pattern that detects the capability.

Modes:
  --diff              Diff BASE_SHA..HEAD for changed files (default; also
                      CAPABILITY_BOUNDARY_MODE=diff).
  --files-from-stdin  Read changed file paths from stdin (also
                      CAPABILITY_BOUNDARY_MODE=files-from-stdin).

Environment:
  BASE_SHA                Base commit to diff HEAD against (default: origin/main).
  ALLOWLIST               Path to TOML allowlist file (default:
                          scripts/capability-boundary-allowlist.toml).
  CAPABILITY_BOUNDARY_MODE
                          Override mode selection.
EOF
}

if [ "$#" -gt 1 ]; then
    usage >&2
    exit 2
fi

if [ "$#" -eq 1 ]; then
    case "$1" in
        --diff)
            MODE=diff
            ;;
        --files-from-stdin)
            MODE=files-from-stdin
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            exit 2
            ;;
    esac
fi

case "$MODE" in
    diff|files-from-stdin)
        ;;
    *)
        printf 'Unknown CAPABILITY_BOUNDARY_MODE: %s\n' "$MODE" >&2
        usage >&2
        exit 2
        ;;
esac

# ── Configuration validation ─────────────────────────────────────────────

validate_config() {
    if [ -z "$CAPABILITY" ]; then
        printf '::error::CAPABILITY environment variable is required.\n' >&2
        usage >&2
        exit 2
    fi

    if [ -z "$OWNER" ]; then
        printf '::error::OWNER environment variable is required.\n' >&2
        usage >&2
        exit 2
    fi

    if [ -z "$REMEDIATION" ]; then
        printf '::error::REMEDIATION environment variable is required.\n' >&2
        usage >&2
        exit 2
    fi

    if [ -z "$PATTERN" ]; then
        printf '::error::PATTERN environment variable is required.\n' >&2
        usage >&2
        exit 2
    fi

    case "$OWNER" in
        */)
            printf '::error::OWNER must not end with a slash: %s\n' "$OWNER" >&2
            exit 2
            ;;
    esac

    # Validate allowlist only when it exists; a missing allowlist is not fatal
    # so that detectors can run before the file is populated.
    if [ -f "$ALLOWLIST" ]; then
        validate_allowlist
    fi
}

# ── Allowlist parsing and validation ─────────────────────────────────────

# Allowlist schema:
#   [[entries]]
#   capability = "git"
#   path = "server/crates/some-crate/src/foo.rs"
#   matcher = "git2::"
#   owner = "team/name"
#   rationale = "..."
#   expires = "2026-12-31"
#   # OR cleanup_issue = "https://github.com/.../issues/123"

allowlist_error() {
    printf '::error::capability-boundary allowlist (%s): %s\n' "$ALLOWLIST" "$1" >&2
}

allowlist_awk() {
    awk '
        /^[[:space:]]*\[\[entries\]\][[:space:]]*$/ {
            if (entry > 0) validate_entry()
            entry++
            reset()
        }
        function reset() {
            capability=""; path=""; matcher=""; owner=""; rationale=""
            expires=""; cleanup_issue=""
        }
        function has_entry_content() {
            return (capability != "" || path != "" || matcher != "" || \
                    owner != "" || rationale != "" || expires != "" || \
                    cleanup_issue != "")
        }
        /^[[:space:]]*capability[[:space:]]*=/ { capability = extract($0) }
        /^[[:space:]]*path[[:space:]]*=/ { path = extract($0) }
        /^[[:space:]]*matcher[[:space:]]*=/ { matcher = extract($0) }
        /^[[:space:]]*owner[[:space:]]*=/ { owner = extract($0) }
        /^[[:space:]]*rationale[[:space:]]*=/ { rationale = extract($0) }
        /^[[:space:]]*expires[[:space:]]*=/ { expires = extract($0) }
        /^[[:space:]]*cleanup_issue[[:space:]]*=/ { cleanup_issue = extract($0) }
        function extract(line) {
            sub(/^[[:space:]]*[^[:space:]]+[[:space:]]*=[[:space:]]*/, "", line)
            # strip surrounding quotes if present
            if (substr(line, 1, 1) == "\"" && substr(line, length(line), 1) == "\"") {
                line = substr(line, 2, length(line) - 2)
            }
            # unescape TOML \" sequences
            gsub(/\\\"/, "\"", line)
            return line
        }
        END {
            if (entry == 0) {
                # Empty or no entries is allowed.
                exit 0
            }
            validate_entry()
        }
        function validate_entry() {
            if (!has_entry_content()) return
            if (capability == "" || path == "" || matcher == "" || owner == "" || rationale == "") {
                print "missing required fields in allowlist entry (need capability, path, matcher, owner, rationale)"
                exit 2
            }
            if (expires == "" && cleanup_issue == "") {
                print "entry must have either expires or cleanup_issue"
                exit 2
            }
            # Forbidden broad globs: server/crates/** is the canonical example.
            # Synthetic fixture globs under server/crates/*_guard_fixture/** are
            # explicitly permitted for self-tests.
            if (path == "server/crates/**" || path ~ /^server\/crates\/\*\*\//) {
                if (!(path ~ /^server\/crates\/[^\/]+_guard_fixture\/\*\*$/)) {
                    print "forbidden broad glob: " path
                    exit 2
                }
            }
        }
    ' "$1"
}

validate_allowlist() {
    if [ ! -r "$ALLOWLIST" ]; then
        allowlist_error "file is not readable: $ALLOWLIST"
        exit 2
    fi

    msg=$(allowlist_awk "$ALLOWLIST" 2>&1) || {
        allowlist_error "$msg"
        exit 2
    }
}

# is_path_allowed_by_allowlist checks the loaded allowlist for an exact match
# of this capability, path, and matcher.  If the allowlist does not exist, the
# path is considered not allowed.  The matcher value must be present in the
# matching line so reviewers can audit the precise pattern being exempted.
is_path_allowed_by_allowlist() {
    cap=$1
    path=$2
    match_text=$3

    if [ ! -f "$ALLOWLIST" ]; then
        return 1
    fi

    awk -v cap="$cap" -v path="$path" -v match_text="$match_text" '
        /^[[:space:]]*\[\[entries\]\][[:space:]]*$/ { entry++; reset() }
        function reset() { capability=""; path_=""; matcher=""; in_entry=0 }
        /^[[:space:]]*capability[[:space:]]*=/ { capability = extract($0); in_entry=1 }
        /^[[:space:]]*path[[:space:]]*=/ { path_ = extract($0) }
        /^[[:space:]]*matcher[[:space:]]*=/ { matcher = extract($0) }
        function extract(line) {
            sub(/^[[:space:]]*[^[:space:]]+[[:space:]]*=[[:space:]]*/, "", line)
            if (substr(line, 1, 1) == "\"" && substr(line, length(line), 1) == "\"") {
                line = substr(line, 2, length(line) - 2)
            }
            # unescape TOML \" sequences
            gsub(/\\\"/, "\"", line)
            return line
        }
        in_entry && capability == cap && path_ == path && matcher == match_text {
            print "allowed"
            exit 0
        }
        END { exit 1 }
    ' "$ALLOWLIST" | grep -q "^allowed$"
}

# ── Scope / filter helpers ─────────────────────────────────────────────

# is_in_scope_file returns 0 if the path is a Rust source file we should
# inspect.  Generated paths, docs, deployment manifests, and the owner crate
# are excluded.  Build/runtime scripts are intentionally left as an extension
# point (not scanned by default).
is_in_scope_file() {
    path=$1

    # Must be a Rust source file (real .rs files, not module paths).
    case "$path" in
        *.rs)
            ;;
        *)
            return 1
            ;;
    esac

    # Must be under the server source tree.
    case "$path" in
        server/crates/*.rs|server/src/*.rs)
            ;;
        *)
            return 1
            ;;
    esac

    # Exclude the owner crate — it is allowed to use this capability directly.
    case "$path" in
        "$OWNER"/*)
            return 1
            ;;
    esac

    # Exclude generated/vendor/target output and docs.
    case "$path" in
        */generated/*|*.gen.*|*/vendor/*|*/target/*|docs/*|*.md)
            return 1
            ;;
    esac

    # Exclude deployment manifests.
    case "$path" in
        */deploy/*|*/k8s/*|*/helm/*|*/terraform/*|*.yaml|*.yml|*.json)
            return 1
            ;;
    esac

    return 0
}

# is_comment_or_doc returns 0 if the given line is a comment or doc line only.
is_comment_or_doc() {
    line=$1

    # Strip leading whitespace.
    trimmed=${line#"${line%%[![:space:]]*}"}

    case "$trimmed" in
        "//"*|"/*"*|"*/"*|"*"*)
            return 0
            ;;
    esac

    return 1
}

# ── Diagnostic helpers ─────────────────────────────────────────────────

emit_diagnostic() {
    file=$1
    line_no=$2
    matcher=$3

    printf '::error::file=%s,line=%s::%s capability usage outside %s (matcher: %s). Remediation owner: %s.\n' \
        "$file" "$line_no" "$CAPABILITY" "$OWNER" "$matcher" "$REMEDIATION" >&2
}

emit_summary() {
    checked=$1
    violations=$2

    if [ "$checked" -eq 0 ]; then
        printf 'Checked 0 source file(s); no %s boundary violations.\n' "$CAPABILITY"
        return 0
    fi

    if [ "$violations" -gt 0 ]; then
        printf '\n' >&2
        printf '::error::Found %d file(s) with %s capability usage outside %s/. All %s usage must go through %s.\n' \
            "$violations" "$CAPABILITY" "$OWNER" "$CAPABILITY" "$REMEDIATION" >&2
        return 1
    fi

    printf 'OK: checked %d source file(s); no %s boundary violations.\n' "$checked" "$CAPABILITY"
    return 0
}

# ── Core file checking ─────────────────────────────────────────────────

check_files() {
    violations=0
    checked=0

    while IFS= read -r file || [ -n "$file" ]; do
        [ -n "$file" ] || continue

        # Strip leading ./ if present.
        case "$file" in
            ./*) file=${file#./} ;;
        esac

        is_in_scope_file "$file" || continue
        # Skip deleted / nonexistent files.
        [ -f "$file" ] || continue

        checked=$((checked + 1))

        # Grep the file for capability patterns.  We intentionally do not use
        # -o so that line numbers remain valid.
        hits=$(grep -E -n "$PATTERN" "$file" 2>/dev/null || true)
        if [ -z "$hits" ]; then
            continue
        fi

        # For each hit, check whether the line is comments-only and whether
        # the file is allowlisted for this exact matcher.
        in_file_violations=0
        # Use a temporary file to avoid subshell variable scoping issues with
        # POSIX while-read loops.  This preserves the violation count across
        # hits so that per-file and total violation counters are accurate.
        tmp_hits=$(mktemp "${LOG_DIR:-/var/tmp}/cbg-hits.XXXXXX" 2>/dev/null || \
                   mktemp "${HOME:-/tmp}/cbg-hits.XXXXXX")
        printf '%s\n' "$hits" > "$tmp_hits"
        while IFS= read -r hit; do
            [ -n "$hit" ] || continue

            line_no=${hit%%:*}
            line_text=${hit#*:}

            if is_comment_or_doc "$line_text"; then
                continue
            fi

            # Identify the matcher substring that matched.  We try the obvious
            # literal anchors first; if they fail, fall back to the raw line.
            # NOTE: more specific patterns MUST come before less specific ones
            # (e.g. tokio::process::Command::new("git") before Command::new("git"))
            # because shell case uses first-match-wins.
            matcher=$line_text
            case "$line_text" in
                *"git2::"*) matcher="git2::" ;;
                *"use git2"*) matcher="use git2" ;;
                *'tokio::process::Command::new("git")'*) matcher='tokio::process::Command::new("git")' ;;
                *'Command::new("git")'*) matcher='Command::new("git")' ;;
                *'::new("git")'*) matcher='Command::new("git")' ;;
                *"reqwest::ClientBuilder"*) matcher="reqwest::ClientBuilder" ;;
                *"reqwest::RequestBuilder"*) matcher="reqwest::RequestBuilder" ;;
                *"reqwest::Client"*) matcher="reqwest::Client" ;;
                *"reqwest::{"*) matcher="reqwest::{" ;;
                *"kube::"*) matcher="kube::" ;;
                *"use kube"*) matcher="use kube" ;;
                *"k8s_openapi"*) matcher="k8s_openapi" ;;
                *'tokio::process::Command::new("kubectl")'*) matcher='tokio::process::Command::new("kubectl")' ;;
                *'Command::new("kubectl")'*) matcher='Command::new("kubectl")' ;;
                *'::new("kubectl")'*) matcher='Command::new("kubectl")' ;;
            esac

            if is_path_allowed_by_allowlist "$CAPABILITY" "$file" "$matcher"; then
                continue
            fi

            in_file_violations=$((in_file_violations + 1))
            emit_diagnostic "$file" "$line_no" "$matcher"
        done < "$tmp_hits"
        rm -f -- "$tmp_hits"

        if [ "$in_file_violations" -gt 0 ]; then
            violations=$((violations + 1))
        fi
    done

    emit_summary "$checked" "$violations"
}

# ── Diff mode: collect changed files from git ──────────────────────────

get_changed_files_from_diff() {
    BASE_SHA=${BASE_SHA:-}
    if [ -z "$BASE_SHA" ]; then
        git fetch --no-tags --depth=1 origin main >/dev/null 2>&1 || true
        BASE_SHA=$(git rev-parse --verify origin/main 2>/dev/null || true)
    fi

    if [ -z "$BASE_SHA" ]; then
        printf '::error::check-capability-boundaries: could not determine a base SHA to diff against.\n' >&2
        exit 2
    fi

    printf 'Checking %s boundary against base %s ...\n' "$CAPABILITY" "$BASE_SHA"

    # Diff to find changed (Added, Modified, Copied) Rust files.  Deleted and
    # renamed-from files are naturally skipped by the subsequent [ -f ] check.
    git diff --name-only --diff-filter=ACMR "$BASE_SHA...HEAD" -- '*.rs' || true
}

# ── Main ───────────────────────────────────────────────────────────────

validate_config

case "$MODE" in
    diff)
        get_changed_files_from_diff | check_files
        ;;
    files-from-stdin)
        check_files
        ;;
esac
