#!/bin/sh
# Git capability-boundary guard.
#
# All direct git capability usage (libgit2 via git2::, direct shell-outs via
# Command::new("git"), and tokio::process::Command::new("git")) MUST live inside
# server/crates/djinn-git/.  Other crates should go through djinn-git's API.
#
# This script delegates to the shared capability-boundary guard plumbing in
# scripts/check-capability-boundaries.sh.
#
# Usage:
#   ./scripts/check-git-boundary.sh
#   BASE_SHA=<sha> ./scripts/check-git-boundary.sh
#   printf '%s\n' path/to/file.rs | ./scripts/check-git-boundary.sh --files-from-stdin
#
# Modes:
#   (default)       Diff BASE_SHA..HEAD to get changed Rust files, then check.
#   --files-from-stdin
#                   Read changed file paths from stdin; check only in-scope files.
#
# Environment:
#   BASE_SHA        Base commit to diff HEAD against (default: origin/main).
#   ALLOWLIST       Optional path to a TOML allowlist (default:
#                   scripts/capability-boundary-allowlist.toml).
#
# Exit codes:
#   0  No violations found (or no in-scope files to check).
#   1  Violations found — git capability usage detected outside djinn-git.
#   2  Usage / configuration error.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

# Git capability configuration.
export CAPABILITY=git
export OWNER=server/crates/djinn-git
export REMEDIATION=djinn-git

# Matchers:
#   git2::                              — libgit2 Rust bindings
#   use git2                            — import statement for git2 crate
#   Command::new("git")                 — std::process::Command shell-out
#   tokio::process::Command::new("git") — tokio async Command shell-out
#   ::new("git")                        — catch-all for aliased Command forms
#                                         (e.g. use std::process::Command as
#                                         ProcessCommand; ProcessCommand::new("git"))
export PATTERN='(git2::|use git2|Command::new\("git"\)|tokio::process::Command::new\("git"\)|::new\("git"\))'

exec sh "$SCRIPT_DIR/check-capability-boundaries.sh" "$@"
