#!/bin/sh
# Regression coverage for the Helm <-> GitHub App credential-source contract.
#
# Run from any directory:
#
#   sh scripts/test-helm-github-app-render.sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
CHART="$REPO_ROOT/deploy/helm/djinn"
LOCAL_VALUES="$CHART/values.local.yaml"

command -v helm >/dev/null 2>&1 || {
    printf 'FATAL: helm is required\n' >&2
    exit 2
}

umask 077
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/djinn-helm-github-app.XXXXXX")
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

render() {
    output=$1
    shift
    helm template djinn "$CHART" --namespace djinn "$@" > "$output"
}

assert_contains() {
    label=$1
    needle=$2
    file=$3
    if ! grep -Fq -- "$needle" "$file"; then
        printf 'FAIL: %s (missing %s)\n' "$label" "$needle" >&2
        exit 1
    fi
    printf 'ok: %s\n' "$label"
}

assert_lacks() {
    label=$1
    needle=$2
    file=$3
    if grep -Fq -- "$needle" "$file"; then
        printf 'FAIL: %s (unexpected %s)\n' "$label" "$needle" >&2
        exit 1
    fi
    printf 'ok: %s\n' "$label"
}

fresh="$TMP_DIR/fresh-local.yaml"
render "$fresh" --values "$LOCAL_VALUES"
assert_lacks 'fresh local omits chart-managed GitHub App Secret' \
    '# Source: djinn/templates/secret-github-app.yaml' "$fresh"
assert_lacks 'fresh local omits GitHub App env variables and private-key path' \
    'GITHUB_APP_' "$fresh"
assert_lacks 'fresh local omits GitHub App volume and mount' \
    'name: github-app' "$fresh"
assert_contains 'fresh local enables self-setup' \
    'DJINN_ENABLE_SELF_SETUP: "true"' "$fresh"
assert_contains 'fresh local allows personal-account installation' \
    'DJINN_ALLOW_USER_INSTALLATIONS: "true"' "$fresh"

partial="$TMP_DIR/partial-inline.yaml"
render "$partial" --values "$LOCAL_VALUES" \
    --set-string secrets.githubApp.appId=12345
assert_contains 'partial inline config renders the chart-managed Secret' \
    '# Source: djinn/templates/secret-github-app.yaml' "$partial"
assert_contains 'partial inline config injects App ID' \
    'name: GITHUB_APP_ID' "$partial"
assert_contains 'partial inline config injects client ID' \
    'name: GITHUB_APP_CLIENT_ID' "$partial"
assert_contains 'partial inline config injects client secret' \
    'name: GITHUB_APP_CLIENT_SECRET' "$partial"
assert_contains 'partial inline config injects private-key path' \
    'name: GITHUB_APP_PRIVATE_KEY_PATH' "$partial"
assert_contains 'partial inline config mounts GitHub App Secret' \
    'mountPath: /var/run/secrets/djinn/github-app' "$partial"

numeric_zero="$TMP_DIR/numeric-zero-app-id.yaml"
render "$numeric_zero" --values "$LOCAL_VALUES" \
    --set secrets.githubApp.appId=0
assert_contains 'numeric zero App ID is still an attempted configuration' \
    '# Source: djinn/templates/secret-github-app.yaml' "$numeric_zero"
assert_contains 'numeric zero App ID keeps the fatal-validation env surface' \
    'name: GITHUB_APP_ID' "$numeric_zero"

existing="$TMP_DIR/existing-secret.yaml"
render "$existing" --values "$LOCAL_VALUES" \
    --set-string secrets.githubApp.existingSecret=precreated-github-app
assert_lacks 'existingSecret bypasses chart-managed Secret rendering' \
    '# Source: djinn/templates/secret-github-app.yaml' "$existing"
assert_contains 'existingSecret name is preserved on the Deployment' \
    'secretName: precreated-github-app' "$existing"
assert_contains 'existingSecret still injects fatal-validation env surface' \
    'name: GITHUB_APP_ID' "$existing"
assert_contains 'existingSecret still mounts private-key path' \
    'mountPath: /var/run/secrets/djinn/github-app' "$existing"

defaults="$TMP_DIR/defaults.yaml"
render "$defaults"
assert_contains 'production default disables self-setup' \
    'DJINN_ENABLE_SELF_SETUP: "false"' "$defaults"
assert_contains 'production default remains organization-only' \
    'DJINN_ALLOW_USER_INSTALLATIONS: "false"' "$defaults"

printf 'all Helm GitHub App render assertions passed\n'
