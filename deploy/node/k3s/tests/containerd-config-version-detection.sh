#!/usr/bin/env bash
# Hermetic contract for the containerd CRI configuration-version detector and
# for the two managed templates it selects between.
# Usage: bash deploy/node/k3s/tests/containerd-config-version-detection.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NODE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES="$SCRIPT_DIR/fixtures/containerd"
DETECTOR="$NODE_DIR/containerd-config-version.sh"
V2_TEMPLATE="$NODE_DIR/containerd/config.toml.tmpl"
V3_TEMPLATE="$NODE_DIR/containerd/config-v3.toml.tmpl"

failures=0
fail() { printf 'FAIL: %s\n' "$*" >&2; failures=$((failures + 1)); }
ok() { printf 'ok %s\n' "$*"; }

expect_eq() {
  local what=$1 expected=$2 actual=$3
  if [ "$expected" = "$actual" ]; then
    ok "$what"
  else
    fail "$what: expected [$expected], got [$actual]"
  fi
}

# shellcheck source=../containerd-config-version.sh
. "$DETECTOR"

expect_version() {
  local fixture=$1 expected=$2 actual
  actual=$(djinn_containerd_detect_version "$FIXTURES/$fixture" 2>/dev/null) || actual='<error>'
  expect_eq "detect $fixture" "$expected" "$actual"
}

expect_unresolvable() {
  local fixture=$1
  if djinn_containerd_detect_version "$FIXTURES/$fixture" >/dev/null 2>&1; then
    fail "detect $fixture: resolved a version it must reject"
  else
    ok "detect $fixture rejected"
  fi
}

# The generated file's own top-level version key is the only input.
expect_version live-v3-vps.toml 3
expect_version live-v3-vps-preinstall.toml 3
expect_version live-v2-k3s.toml 2
expect_version live-v2-k3s-preinstall.toml 2
expect_version detect-no-version-key.toml 2
expect_version detect-version-nested-only.toml 2
expect_unresolvable detect-version-4.toml
expect_unresolvable detect-version-malformed.toml

# A contradicting k3s/containerd version string never changes the answer.
expect_version detect-v3-with-contradicting-k3s-strings.toml 3
expect_version detect-v2-with-contradicting-k3s-strings.toml 2

# A missing live configuration declares no version key, exactly like an
# existing file without one.
expect_version does-not-exist.toml 2

# The tuple each version selects.
expect_eq 'namespace for 2' 'io.containerd.grpc.v1.cri' "$(djinn_containerd_namespace_for_version 2)"
expect_eq 'namespace for 3' 'io.containerd.cri.v1.runtime' "$(djinn_containerd_namespace_for_version 3)"
expect_eq 'template for 2' 'config.toml.tmpl' "$(djinn_containerd_template_basename_for_version 2)"
expect_eq 'template for 3' 'config-v3.toml.tmpl' "$(djinn_containerd_template_basename_for_version 3)"
if djinn_containerd_namespace_for_version 4 >/dev/null 2>&1; then
  fail 'namespace for 4: resolved an unsupported version'
else
  ok 'namespace for 4 rejected'
fi
if djinn_containerd_template_basename_for_version 4 >/dev/null 2>&1; then
  fail 'template for 4: resolved an unsupported version'
else
  ok 'template for 4 rejected'
fi

# Template and validator cannot drift: the runtime-table header the detector
# returns must be byte-equal, quoting included, to the header line the selected
# template actually declares.
assert_header_in_template() {
  local version=$1 template=$2 header
  header=$(djinn_containerd_runtime_table_for_version "$version")
  if grep -Fxq "$header" "$template"; then
    ok "version $version header is byte-equal in $(basename "$template")"
  else
    fail "version $version header [$header] is not a line of $template"
  fi
}
expect_eq 'table for 2' \
  '[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc-cgroupwritable]' \
  "$(djinn_containerd_runtime_table_for_version 2)"
expect_eq 'table for 3' \
  "[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc-cgroupwritable]" \
  "$(djinn_containerd_runtime_table_for_version 3)"
assert_header_in_template 2 "$V2_TEMPLATE"
assert_header_in_template 3 "$V3_TEMPLATE"

# Each template declares only its own generation's namespace.
if grep -Fq "$(djinn_containerd_runtime_table_for_version 3)" "$V2_TEMPLATE"; then
  fail 'v2 template declares the v3 runtime table'
else
  ok 'v2 template declares no v3 runtime table'
fi
if grep -Fq "$(djinn_containerd_runtime_table_for_version 2)" "$V3_TEMPLATE"; then
  fail 'v3 template declares the v2 runtime table'
else
  ok 'v3 template declares no v2 runtime table'
fi

# The v3 template keeps the base template and the exact handler body, and
# matches the base runc handler's systemd cgroup driver.
v3_text=$(cat "$V3_TEMPLATE")
case "$v3_text" in
  *'{{ template "base" . }}'*) ok 'v3 template retains the k3s base template' ;;
  *) fail 'v3 template dropped {{ template "base" . }}' ;;
esac
v3_body=$(awk -v header="$(djinn_containerd_runtime_table_for_version 3)" '
  $0 == header { in_table=1 }
  in_table {
    if (seen && /^\[/) exit
    seen=1
    sub(/[[:space:]]*#.*/, "")
    if ($0 !~ /^[[:space:]]*$/) print
  }
' "$V3_TEMPLATE")
expect_eq 'v3 handler body' \
  "$(printf '%s\n  runtime_type = "io.containerd.runc.v2"\n  cgroup_writable = true' \
    "$(djinn_containerd_runtime_table_for_version 3)")" \
  "$v3_body"
v3_options=$(awk '
  $0 == "[plugins.'"'"'io.containerd.cri.v1.runtime'"'"'.containerd.runtimes.runc-cgroupwritable.options]" { in_table=1 }
  in_table {
    if (seen && /^\[/) exit
    seen=1
    sub(/[[:space:]]*#.*/, "")
    if ($0 !~ /^[[:space:]]*$/) print
  }
' "$V3_TEMPLATE")
expect_eq 'v3 handler options' \
  "$(printf "[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc-cgroupwritable.options]\n  SystemdCgroup = true")" \
  "$v3_options"
# The live node's base runc handler uses the systemd driver; the delegated
# handler must not silently differ from it.
base_options=$(awk '
  $0 == "[plugins.'"'"'io.containerd.cri.v1.runtime'"'"'.containerd.runtimes.runc.options]" { in_table=1 }
  in_table {
    if (seen && /^\[/) exit
    seen=1
    if ($0 !~ /^[[:space:]]*$/) print
  }
' "$FIXTURES/live-v3-vps.toml")
expect_eq 'v3 fixture base runc options' \
  "$(printf "[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc.options]\n  SystemdCgroup = true")" \
  "$base_options"
case "$v3_options" in
  *'SystemdCgroup = true'*) ok 'delegated handler matches the base systemd cgroup driver' ;;
  *) fail 'delegated handler does not set SystemdCgroup = true' ;;
esac

# The executable form prints the whole tuple and fails closed.
cli_out=$(bash "$DETECTOR" "$FIXTURES/live-v3-vps.toml")
expect_eq 'cli tuple for the live v3 node' \
  "$(printf 'version=3\nnamespace=io.containerd.cri.v1.runtime\ntemplate=config-v3.toml.tmpl\ntable=%s' \
    "[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc-cgroupwritable]")" \
  "$cli_out"
cli_out=$(bash "$DETECTOR" "$FIXTURES/live-v2-k3s.toml")
expect_eq 'cli tuple for a v2 node' \
  "$(printf 'version=2\nnamespace=io.containerd.grpc.v1.cri\ntemplate=config.toml.tmpl\ntable=%s' \
    '[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc-cgroupwritable]')" \
  "$cli_out"
if bash "$DETECTOR" "$FIXTURES/detect-version-4.toml" >/dev/null 2>&1; then
  fail 'cli accepted an unsupported version'
else
  ok 'cli rejected an unsupported version'
fi

if [ "$failures" -ne 0 ]; then
  printf 'FAIL: %s containerd config version detection assertion(s)\n' "$failures" >&2
  exit 1
fi
echo 'PASS: containerd config version detection and template selection'
