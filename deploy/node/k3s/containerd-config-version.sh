#!/usr/bin/env bash
# Resolve the containerd CRI configuration generation of a managed k3s node.
#
# The single source of truth is the top-level `version` key of the LIVE
# GENERATED containerd configuration. A k3s or containerd version string is
# never consulted: such strings live in comments, unit files and `k3s --version`
# output, they routinely disagree with the file on a node that has been
# upgraded in place, and the generated file is the only artefact containerd
# actually parses.
#
#   version = 2, or no top-level version key (the containerd v1.x default)
#       namespace io.containerd.grpc.v1.cri   template config.toml.tmpl
#   version = 3
#       namespace io.containerd.cri.v1.runtime  template config-v3.toml.tmpl
#   any other value
#       unresolvable: the caller must install nothing and restart nothing.
#
# A missing or unreadable live configuration has no version key and therefore
# resolves to the containerd v1.x default, exactly as an existing file without
# the key does. Readability of the live file is separately enforced by the
# caller when it validates the rendered result.
#
# Source this file to obtain the functions, or execute it to print the tuple:
#   containerd-config-version.sh /var/lib/rancher/k3s/agent/etc/containerd/config.toml

DJINN_CONTAINERD_HANDLER=${DJINN_CONTAINERD_HANDLER:-runc-cgroupwritable}

# Print the top-level `version` value of a generated containerd configuration,
# or the empty string when the file declares none. Keys inside any table are
# out of scope: TOML places top-level keys before the first table header, so
# scanning stops at the first `[`.
djinn_containerd_config_version_key() {
  local live=$1
  [ -r "$live" ] || return 0
  awk '
    /^[[:space:]]*#/ { next }
    /^[[:space:]]*\[/ { exit }
    /^[[:space:]]*version[[:space:]]*=/ {
      line = $0
      sub(/^[[:space:]]*version[[:space:]]*=/, "", line)
      sub(/#.*/, "", line)
      gsub(/[[:space:]]/, "", line)
      gsub(/"/, "", line)
      print line
      exit
    }
  ' "$live"
}

# Resolve the supported generation (2 or 3) from the live configuration alone.
djinn_containerd_detect_version() {
  local live=$1 key
  key=$(djinn_containerd_config_version_key "$live") || return 1
  case "$key" in
    '' | 2) printf '2\n' ;;
    3) printf '3\n' ;;
    *)
      printf 'unsupported containerd config version %s in %s\n' "$key" "$live" >&2
      return 1
      ;;
  esac
}

djinn_containerd_namespace_for_version() {
  case "$1" in
    2) printf 'io.containerd.grpc.v1.cri\n' ;;
    3) printf 'io.containerd.cri.v1.runtime\n' ;;
    *) printf 'unsupported containerd config version: %s\n' "$1" >&2; return 1 ;;
  esac
}

djinn_containerd_template_basename_for_version() {
  case "$1" in
    2) printf 'config.toml.tmpl\n' ;;
    3) printf 'config-v3.toml.tmpl\n' ;;
    *) printf 'unsupported containerd config version: %s\n' "$1" >&2; return 1 ;;
  esac
}

# The exact runtime-table header line that both the managed template and the
# rendered live configuration must carry. containerd v1.x renders plugin path
# segments with double quotes and containerd 2.x with single quotes; the quoting
# is part of the literal line the validator compares, so it is produced here
# rather than reconstructed by each caller.
djinn_containerd_runtime_table_for_version() {
  local namespace quote
  namespace=$(djinn_containerd_namespace_for_version "$1") || return 1
  case "$1" in
    2) quote='"' ;;
    3) quote="'" ;;
  esac
  printf '[plugins.%s%s%s.containerd.runtimes.%s]\n' \
    "$quote" "$namespace" "$quote" "$DJINN_CONTAINERD_HANDLER"
}

djinn_containerd_config_version_main() {
  local live=$1 version
  [ -n "$live" ] || {
    printf 'usage: containerd-config-version.sh LIVE_CONFIG_PATH\n' >&2
    return 2
  }
  version=$(djinn_containerd_detect_version "$live") || return 1
  printf 'version=%s\n' "$version"
  printf 'namespace=%s\n' "$(djinn_containerd_namespace_for_version "$version")"
  printf 'template=%s\n' "$(djinn_containerd_template_basename_for_version "$version")"
  printf 'table=%s\n' "$(djinn_containerd_runtime_table_for_version "$version")"
}

if [ "${BASH_SOURCE[0]:-$0}" = "$0" ]; then
  set -eu
  djinn_containerd_config_version_main "${1:-}"
fi
