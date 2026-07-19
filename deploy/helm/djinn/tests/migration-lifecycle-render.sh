#!/usr/bin/env bash
# Helm render contract for caller-owned PostgreSQL bootstrap and migration.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TMPDIR_RENDER=$(mktemp -d)
trap 'rm -rf "$TMPDIR_RENDER"' EXIT

command -v helm >/dev/null 2>&1 || {
    echo "FAIL: required test tool 'helm' is not installed" >&2
    exit 1
}
command -v python3 >/dev/null 2>&1 || {
    echo "FAIL: required test tool 'python3' is not installed" >&2
    exit 1
}

if helm template migration-test "$CHART_DIR" \
    --show-only templates/deployment-server.yaml \
    > "$TMPDIR_RENDER/missing-secret.out" 2>&1; then
    echo "FAIL: fresh install rendered without migration.designatedOperatorSecret" >&2
    exit 1
fi
grep -Fq \
    'migration.designatedOperatorSecret is required on fresh installs' \
    "$TMPDIR_RENDER/missing-secret.out"

render() {
    local output=$1
    shift
    helm template migration-test "$CHART_DIR" \
        --show-only templates/deployment-server.yaml \
        "$@" > "$output"
}

render "$TMPDIR_RENDER/install-bundled.yaml" \
    --set-string migration.designatedOperatorSecret=operator-identity
render "$TMPDIR_RENDER/install-external.yaml" \
    --set-string migration.designatedOperatorSecret=operator-identity \
    --set postgres.enabled=false \
    --set-string database.existingSecret=database-url \
    --set-string 'extraEnvFrom[0].secretRef.name=shared-environment'
render "$TMPDIR_RENDER/upgrade-bundled.yaml" --is-upgrade
render "$TMPDIR_RENDER/upgrade-external.yaml" \
    --is-upgrade \
    --set postgres.enabled=false \
    --set-string database.existingSecret=database-url \
    --set-string 'extraEnvFrom[0].secretRef.name=shared-environment'

python3 - "$TMPDIR_RENDER" <<'PY'
from pathlib import Path
import sys

root = Path(sys.argv[1])
install_bundled = (root / "install-bundled.yaml").read_text(encoding="utf-8")
install_external = (root / "install-external.yaml").read_text(encoding="utf-8")
upgrade_bundled = (root / "upgrade-bundled.yaml").read_text(encoding="utf-8")
upgrade_external = (root / "upgrade-external.yaml").read_text(encoding="utf-8")

for rendered in (install_bundled, install_external):
    assert rendered.index("name: bootstrap-designated-operator") < rendered.index("name: migrate")
    assert rendered.count("name: DJINN_MIGRATION_DESIGNATED_OPERATOR_USER_ID") == 2
    assert rendered.count("name: DJINN_BOOTSTRAP_DESIGNATED_OPERATOR_GITHUB_ID") == 1
    assert rendered.count("name: DJINN_BOOTSTRAP_DESIGNATED_OPERATOR_GITHUB_LOGIN") == 1

# Bootstrap, migrate, and app all receive the external URL and extra env source.
assert install_external.count("name: DJINN_DATABASE_URL") == 3
assert install_external.count("name: shared-environment") == 3

for rendered in (upgrade_bundled, upgrade_external):
    assert "bootstrap-designated-operator" not in rendered
    assert "DJINN_MIGRATION_DESIGNATED_OPERATOR_USER_ID" not in rendered
    assert "DJINN_BOOTSTRAP_DESIGNATED_OPERATOR_" not in rendered
    assert "operator-identity" not in rendered

# Upgrade has only migrate and app containers, both with the external sources.
assert upgrade_external.count("name: DJINN_DATABASE_URL") == 2
assert upgrade_external.count("name: shared-environment") == 2
PY

echo "=== PostgreSQL migration lifecycle Helm render tests passed ==="
