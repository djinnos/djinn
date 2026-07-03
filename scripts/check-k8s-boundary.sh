#!/bin/sh
# Kubernetes capability-boundary guard.
#
# All direct Kubernetes client/object access (the `kube` crate via kube:: and
# use kube, the `k8s_openapi` crate, and direct kubectl shell-outs via
# Command::new("kubectl") and tokio::process::Command::new("kubectl")) MUST
# live inside server/crates/djinn-k8s/.  Other crates should go through
# djinn-k8s's API.
#
# This script delegates to the shared capability-boundary guard plumbing in
# scripts/check-capability-boundaries.sh.
#
# Usage:
#   ./scripts/check-k8s-boundary.sh
#   BASE_SHA=<sha> ./scripts/check-k8s-boundary.sh
#   printf '%s\n' path/to/file.rs | ./scripts/check-k8s-boundary.sh --files-from-stdin
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
#   1  Violations found — k8s capability usage detected outside djinn-k8s.
#   2  Usage / configuration error.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

# Kubernetes capability configuration.
export CAPABILITY=k8s
export OWNER=server/crates/djinn-k8s
export REMEDIATION=djinn-k8s

# Matchers:
#   kube::                              — Kubernetes Rust client crate types
#   use kube                            — import statement for the kube crate
#   k8s_openapi                         — Kubernetes OpenAPI type definitions
#   Command::new("kubectl")             — std::process::Command shell-out
#   tokio::process::Command::new("kubectl") — tokio async Command shell-out
#   ::new("kubectl")                    — catch-all for aliased Command forms
#                                         (e.g. use std::process::Command as
#                                         ProcessCommand; ProcessCommand::new("kubectl"))
export PATTERN='(kube::|use kube|k8s_openapi|Command::new\("kubectl"\)|tokio::process::Command::new\("kubectl"\)|::new\("kubectl"\))'

exec sh "$SCRIPT_DIR/check-capability-boundaries.sh" "$@"
