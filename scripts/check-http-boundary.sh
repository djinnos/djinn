#!/bin/sh
# HTTP capability-boundary guard.
#
# All direct outbound HTTP client construction/imports (reqwest::Client,
# reqwest::ClientBuilder, reqwest::RequestBuilder) MUST live inside
# server/crates/djinn-provider/.  Other crates should go through
# djinn-provider's API.  Pure reqwest::StatusCode usage is not flagged.
#
# This script delegates to the shared capability-boundary guard plumbing in
# scripts/check-capability-boundaries.sh.
#
# Usage:
#   ./scripts/check-http-boundary.sh
#   BASE_SHA=<sha> ./scripts/check-http-boundary.sh
#   printf '%s\n' path/to/file.rs | ./scripts/check-http-boundary.sh --files-from-stdin
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
#   1  Violations found — HTTP capability usage detected outside djinn-provider.
#   2  Usage / configuration error.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

# HTTP capability configuration.
export CAPABILITY=http
export OWNER=server/crates/djinn-provider
export REMEDIATION=djinn-provider

# Matchers:
#   reqwest::Client          — fully-qualified Client type/constructor
#   reqwest::ClientBuilder   — fully-qualified ClientBuilder type
#   reqwest::RequestBuilder  — fully-qualified RequestBuilder type
#   reqwest::{               — destructured reqwest import (catches combined
#                               imports like `use reqwest::{Client, StatusCode}`)
#
# Pure reqwest::StatusCode and reqwest::header imports are NOT flagged — the
# high-signal policy targets outbound client construction/imports only.
# Destructured imports are caught via `reqwest::{` so that combined imports
# like `use reqwest::{Client, StatusCode}` are detected while bare
# `use reqwest::StatusCode` is not.
export PATTERN='(reqwest::Client|reqwest::ClientBuilder|reqwest::RequestBuilder|reqwest::\{)'

exec sh "$SCRIPT_DIR/check-capability-boundaries.sh" "$@"
