#!/usr/bin/env bash
# The retention arm of the launcher-authority cutover, against a REAL registry.
#
# WHY THIS EXISTS
# ---------------
# `ResizeRollout::probe_retention` (task `eeky`) proves a retained rollback
# digest is still pullable by fetching its manifest and hashing the bytes it
# gets back. The Rust suite drives that over a real socket against an
# in-process OCI-manifest endpoint, which settles the *logic*. What it cannot
# settle is whether a real registry — content-type negotiation, the
# `Docker-Content-Digest` header, `manifests/<digest>` addressing, a 404 after a
# delete — behaves the way the probe assumes.
#
# So this suite stands up a disposable `registry:2`, pushes a real image,
# resolves its real manifest digest, and asserts three things:
#
#   1. a retained digest fetched from the registry hashes to itself;
#   2. deleting the manifest — and NOTHING else — turns the check red;
#   3. a digest that was never pushed is red from the start.
#
# (2) is the mutation the criterion names. No catalog row and no `pullable`
# column is touched by it, because the check never reads one: there is nothing
# in this path a stored flag could satisfy.
#
# ISOLATION
# ---------
# Several agents share this machine and a kind cluster with its own registry is
# frequently running. This suite therefore binds its OWN container name and its
# OWN port, both overridable, and deletes the container on exit whether the run
# passed or failed. It never touches a registry it did not create.
#
# USAGE
#   bash deploy/preflight/tests/task-run-resize-rollout.sh
#
# OVERRIDES
#   REG_NAME  container name              (default djinn-eeky-retention-reg)
#   REG_PORT  host port for the registry  (default 5099)
#   SKIP_IF_NO_DOCKER=1  exit 0 instead of failing when docker is unavailable
set -euo pipefail

REG_NAME="${REG_NAME:-djinn-eeky-retention-reg}"
REG_PORT="${REG_PORT:-5099}"
REG_HOST="127.0.0.1:${REG_PORT}"
REPO_PATH="djinn-eeky/retained"

pass=0
fail=0

ok()   { printf '  ok    %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '  FAIL  %s\n' "$1" >&2; fail=$((fail + 1)); }
note() { printf '        %s\n' "$1"; }

cleanup() {
  # Pass or fail, the registry goes. `|| true` so a cleanup failure never masks
  # the verdict.
  docker rm -f "$REG_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

if ! command -v docker >/dev/null 2>&1; then
  if [ "${SKIP_IF_NO_DOCKER:-0}" = "1" ]; then
    printf 'SKIP: docker is unavailable and SKIP_IF_NO_DOCKER=1\n'
    exit 0
  fi
  printf 'FAIL: docker is required to stand up a disposable registry\n' >&2
  exit 1
fi

# Disk first. Pulling `registry:2` and pushing a layer onto a full filesystem
# fails in ways that read like a retention defect, and this repository has lost
# hours to exactly that misreading before.
avail_kb="$(df -Pk / | awk 'NR==2 {print $4}')"
printf 'disk: %s KiB available on /\n' "$avail_kb"
if [ "$avail_kb" -lt 2097152 ]; then
  printf 'FAIL: less than 2 GiB free on /; refusing to pull images\n' >&2
  exit 1
fi

# Refuse to reuse someone else's registry on this port.
if docker ps --format '{{.Names}} {{.Ports}}' | grep -v "^${REG_NAME} " | grep -q ":${REG_PORT}->"; then
  printf 'FAIL: port %s is already published by another container; set REG_PORT\n' "$REG_PORT" >&2
  docker ps --format '  {{.Names}} {{.Ports}}' >&2
  exit 1
fi

printf 'starting disposable registry %s on %s\n' "$REG_NAME" "$REG_HOST"
docker rm -f "$REG_NAME" >/dev/null 2>&1 || true
docker run -d --rm \
  --name "$REG_NAME" \
  -p "127.0.0.1:${REG_PORT}:5000" \
  -e REGISTRY_STORAGE_DELETE_ENABLED=true \
  registry:2 >/dev/null

for _ in $(seq 1 60); do
  if curl -fsS "http://${REG_HOST}/v2/" >/dev/null 2>&1; then break; fi
  sleep 0.5
done
curl -fsS "http://${REG_HOST}/v2/" >/dev/null || {
  printf 'FAIL: the disposable registry never became ready\n' >&2
  exit 1
}

# ── push a real artifact and resolve its real manifest digest ───────────────

work="$(mktemp -d)"
trap 'cleanup; rm -rf "$work"' EXIT
printf 'FROM scratch\nCOPY marker /marker\n' >"$work/Dockerfile"
printf 'eeky retention fixture\n' >"$work/marker"
docker build -q -t "${REG_HOST}/${REPO_PATH}:v1" "$work" >/dev/null
docker push -q "${REG_HOST}/${REPO_PATH}:v1" >/dev/null

ACCEPT='application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json'

# The digest the registry itself reports for the tag. Everything below is
# addressed by this digest, never by the tag: a tag is mutable naming and the
# cutover grants compatibility to exact digests only.
digest="$(curl -fsS -D- -o /dev/null \
  -H "Accept: ${ACCEPT}" \
  "http://${REG_HOST}/v2/${REPO_PATH}/manifests/v1" \
  | tr -d '\r' | awk 'tolower($1) == "docker-content-digest:" {print $2}')"

if [ -z "$digest" ]; then
  printf 'FAIL: the registry reported no Docker-Content-Digest for the pushed tag\n' >&2
  exit 1
fi
note "retained digest: $digest"

# ── the check under test, in shell ──────────────────────────────────────────
#
# Mirrors `probe_retention`: GET the manifest by digest, hash the BODY, compare
# to the recorded digest. The header is used only to discover what to retain;
# the verdict comes from the bytes.
probe_retention() {
  local reference="$1" body status observed
  body="$(mktemp)"
  status="$(curl -sS -o "$body" -w '%{http_code}' \
    -H "Accept: ${ACCEPT}" \
    "http://${REG_HOST}/v2/${REPO_PATH}/manifests/${reference}")"
  if [ "$status" != "200" ]; then
    rm -f "$body"
    printf 'unpullable: HTTP %s\n' "$status"
    return 1
  fi
  observed="sha256:$(sha256sum "$body" | awk '{print $1}')"
  rm -f "$body"
  if [ "$observed" != "$reference" ]; then
    printf 'digest mismatch: served %s\n' "$observed"
    return 1
  fi
  printf 'retained\n'
  return 0
}

# 1. The positive control. Without this, (2) below would be indistinguishable
#    from a check that is red for everything.
if probe_retention "$digest" >/dev/null; then
  ok "a pushed manifest fetched by digest hashes to its own digest"
else
  bad "the positive control failed; the rest of this suite proves nothing"
fi

# 3. A digest that was never pushed. Same code path, no registry mutation.
absent="sha256:$(printf 'never pushed' | sha256sum | awk '{print $1}')"
if probe_retention "$absent" >/dev/null 2>&1; then
  bad "a digest that was never pushed must not be reported as retained"
else
  ok "a digest that was never pushed is refused"
fi

# 2. THE MUTATION. Delete the manifest from the registry and change nothing
#    else — no catalog row, no column, no flag.
delete_status="$(curl -sS -o /dev/null -w '%{http_code}' -X DELETE \
  "http://${REG_HOST}/v2/${REPO_PATH}/manifests/${digest}")"
case "$delete_status" in
  202|200) note "manifest deleted (HTTP $delete_status)" ;;
  *)
    bad "could not delete the manifest (HTTP $delete_status); the mutation did not run"
    ;;
esac

if [ "$delete_status" = "202" ] || [ "$delete_status" = "200" ]; then
  if reason="$(probe_retention "$digest" 2>&1)"; then
    bad "deleting the manifest left the retention check green: $reason"
  else
    ok "deleting the manifest turns the retention check red ($reason)"
  fi
fi

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
