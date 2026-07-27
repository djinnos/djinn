#!/usr/bin/env bash
# Hermetic static contract for the bzpt production-activation runbook.
# Usage: bash deploy/runbooks/tests/bzpt-production-activation.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNBOOK="$SCRIPT_DIR/../bzpt-production-activation.md"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  return 1
}

require_line() {
  local document=$1 description=$2 pattern=$3
  grep -Fq -- "$pattern" "$document" || fail "missing $description"
}

first_line() {
  local document=$1 pattern=$2
  grep -nF -- "$pattern" "$document" | head -n1 | cut -d: -f1
}

validate() {
  local document=$1 prerequisite_line activation_line step_line

  require_line "$document" 'd308 observation-window close timestamp field' \
    'D308_OBSERVATION_WINDOW_CLOSE_TIMESTAMP:' || return 1
  require_line "$document" 'selected Helm release revision field' \
    'SELECTED_HELM_RELEASE_REVISION:' || return 1
  require_line "$document" 'deployment outcome field' 'DEPLOYMENT_OUTCOME:' || return 1
  require_line "$document" 'activation deployment outcome value' 'DEPLOYMENT_OUTCOME: activation' || return 1
  require_line "$document" 'rollback deployment outcome value' 'DEPLOYMENT_OUTCOME: rollback' || return 1
  require_line "$document" 'retained upgrade/rollback transcript link field' \
    'RETAINED_UPGRADE_OR_ROLLBACK_TRANSCRIPT_LINK:' || return 1
  require_line "$document" 'retained Helm-history transcript link field' \
    'RETAINED_HELM_HISTORY_TRANSCRIPT_LINK:' || return 1

  require_line "$document" 'closed d308 observation-window prerequisite' \
    'D308_OBSERVATION_WINDOW_STATUS: closed' || return 1
  require_line "$document" 'bzpt production-activation section' \
    '## bzpt production activation procedure' || return 1
  require_line "$document" 'bzpt production-activation step marker' \
    'BZPT_PRODUCTION_ACTIVATION_STEP:' || return 1
  prerequisite_line=$(first_line "$document" 'D308_OBSERVATION_WINDOW_STATUS: closed')
  activation_line=$(first_line "$document" '## bzpt production activation procedure')
  [[ -n "$prerequisite_line" && "$prerequisite_line" -lt "$activation_line" ]] \
    || { fail 'closed d308 prerequisite is not before bzpt production activation'; return 1; }
  while IFS=: read -r step_line _; do
    [[ "$prerequisite_line" -lt "$step_line" ]] \
      || { fail 'closed d308 prerequisite is not before every bzpt production activation step'; return 1; }
  done < <(grep -nF -- 'BZPT_PRODUCTION_ACTIVATION_STEP:' "$document")

  if ! grep -Fq -- 'helm upgrade --atomic --wait' "$document" \
    && ! grep -Fq -- 'helm rollback --wait' "$document"; then
    fail 'successful Helm upgrade or rollback command'
    return 1
  fi
  require_line "$document" 'Helm history command' \
    'helm history "$RELEASE" --namespace "$NAMESPACE"' || return 1
  require_line "$document" 'selected revision deployed requirement' \
    'status `deployed`' || return 1
  require_line "$document" 'explicit independent-delivery boundary' \
    'Implementation, review, merge, and non-production validation remain independent of this production-activation gate.' || return 1
}

expect_rejected() {
  local name=$1 document="$WORK/$1.md"
  shift
  cp "$RUNBOOK" "$document"
  local mutator=$1
  shift
  "$mutator" "$document" "$@"
  if validate "$document" >/dev/null 2>&1; then
    fail "negative fixture '$name' unexpectedly passed"
  fi
}

delete_matching_line() {
  local document=$1 pattern=$2 temporary="$document.tmp"
  awk -v needle="$pattern" 'index($0, needle) == 0' "$document" >"$temporary"
  mv "$temporary" "$document"
}

# The repository document must satisfy the complete static contract without a
# cluster, Helm binary, credentials, or a real production record.
validate "$RUNBOOK"

# Each fixture starts as a private copy. Removing one independent contract must
# make validation fail, proving the positive assertion is not accidental.
expect_rejected ordering delete_matching_line 'D308_OBSERVATION_WINDOW_STATUS: closed'
expect_rejected close-timestamp delete_matching_line 'D308_OBSERVATION_WINDOW_CLOSE_TIMESTAMP:'
expect_rejected selected-revision delete_matching_line 'SELECTED_HELM_RELEASE_REVISION:'
expect_rejected outcome delete_matching_line 'DEPLOYMENT_OUTCOME:'
expect_rejected deployment-transcript delete_matching_line 'RETAINED_UPGRADE_OR_ROLLBACK_TRANSCRIPT_LINK:'
expect_rejected history-transcript delete_matching_line 'RETAINED_HELM_HISTORY_TRANSCRIPT_LINK:'
delete_deployment_commands() {
  local document=$1
  delete_matching_line "$document" 'helm upgrade --atomic --wait'
  delete_matching_line "$document" 'helm rollback --wait'
}

expect_rejected deployment-command delete_deployment_commands
expect_rejected history-command delete_matching_line 'helm history "$RELEASE" --namespace "$NAMESPACE"'
expect_rejected deployed-status delete_matching_line 'status `deployed`'
expect_rejected independence-boundary delete_matching_line \
  'Implementation, review, merge, and non-production validation remain independent of this production-activation gate.'

printf 'PASS: runbook_contract::bzpt_production_activation\n'
