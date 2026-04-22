//! Image-hash computation.
//!
//! The image is cached in GHCR under `djinn-project-<id>:<hash>`. The
//! hash covers everything that could plausibly change the resulting
//! image, so a changed input reliably re-triggers a build:
//!
//! 1. The config JSON, canonicalised — no whitespace, keys sorted.
//! 2. The script-bundle digest. An edit to any `scripts/*.sh` bumps
//!    this, so iterating on an installer doesn't need a manual
//!    `retrigger_image_build` call.
//! 3. The agent-worker helper-image reference. A worker rebuild flows
//!    through here — another gap today's devcontainer-hash scheme has.
//!
//! The hash is sha256 of the concatenated inputs, lowercase hex. It's
//! intentionally 64 chars so log output is readable (cf. the image tag
//! format `djinn-project-<id>:<hash>`).

use sha2::{Digest, Sha256};

use djinn_stack::environment::EnvironmentConfig;

use crate::scripts::{SCRIPTS, ScriptFile};

/// Return the sha256 of the script bundle — the concatenation of every
/// `scripts/*.sh` filename + body, in the stable order that [`SCRIPTS`]
/// already enforces.
pub fn compute_script_bundle_sha() -> String {
    compute_bundle_sha(SCRIPTS)
}

fn compute_bundle_sha(scripts: &[ScriptFile]) -> String {
    let mut hasher = Sha256::new();
    for s in scripts {
        hasher.update(s.name.as_bytes());
        hasher.update([0u8]);
        hasher.update(s.body.as_bytes());
        hasher.update([0u8]);
    }
    hex_lower(&hasher.finalize())
}

/// Compute the image hash. `agent_worker_ref` is the full image
/// reference the Dockerfile will `COPY --from=`. The caller is
/// responsible for threading in a reference that represents the actual
/// worker binary that will ship in the image (e.g. the SHA-pinned tag
/// Tilt publishes to the cluster-local registry).
pub fn compute_environment_hash(
    config: &EnvironmentConfig,
    agent_worker_ref: &str,
) -> String {
    let script_sha = compute_script_bundle_sha();
    let config_json = canonical_json(config);

    let mut hasher = Sha256::new();
    hasher.update(b"env-config/v1\0");
    hasher.update(config_json.as_bytes());
    hasher.update([0u8]);
    hasher.update(script_sha.as_bytes());
    hasher.update([0u8]);
    hasher.update(agent_worker_ref.as_bytes());
    hex_lower(&hasher.finalize())
}

/// serde_json's default serialization ordering matches the struct's
/// field order, which is stable across recompiles (we don't use
/// `HashMap`-backed fields at the top level — `env` is `BTreeMap`,
/// which serializes in key order). Re-parsing + re-emitting via
/// `serde_json::Value` would pick up any field reordering, but given
/// the tight struct definition in `djinn_stack::environment`, the
/// straight serialization is already canonical.
fn canonical_json(config: &EnvironmentConfig) -> String {
    serde_json::to_string(config).expect("EnvironmentConfig serializes")
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> EnvironmentConfig {
        let mut c = EnvironmentConfig::empty();
        c.schema_version = djinn_stack::environment::SCHEMA_VERSION;
        c
    }

    #[test]
    fn hash_is_deterministic_for_same_inputs() {
        let c = cfg();
        let a = compute_environment_hash(&c, "djinn/agent-worker:sha-abc");
        let b = compute_environment_hash(&c, "djinn/agent-worker:sha-abc");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn hash_changes_when_config_changes() {
        let mut c = cfg();
        let a = compute_environment_hash(&c, "djinn/agent-worker:sha-abc");
        c.env.insert("RUST_LOG".into(), "info".into());
        let b = compute_environment_hash(&c, "djinn/agent-worker:sha-abc");
        assert_ne!(a, b);
    }

    #[test]
    fn hash_changes_when_worker_ref_changes() {
        let c = cfg();
        let a = compute_environment_hash(&c, "djinn/agent-worker:sha-abc");
        let b = compute_environment_hash(&c, "djinn/agent-worker:sha-def");
        assert_ne!(a, b);
    }

    #[test]
    fn script_bundle_sha_is_stable() {
        let a = compute_script_bundle_sha();
        let b = compute_script_bundle_sha();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn bundle_sha_detects_body_change() {
        let before = compute_bundle_sha(&[ScriptFile {
            name: "x.sh",
            body: "#!/usr/bin/env bash\necho before\n",
        }]);
        let after = compute_bundle_sha(&[ScriptFile {
            name: "x.sh",
            body: "#!/usr/bin/env bash\necho after\n",
        }]);
        assert_ne!(before, after);
    }

    #[test]
    fn bundle_sha_detects_filename_change() {
        let before = compute_bundle_sha(&[ScriptFile {
            name: "a.sh",
            body: "#!/usr/bin/env bash\n",
        }]);
        let after = compute_bundle_sha(&[ScriptFile {
            name: "b.sh",
            body: "#!/usr/bin/env bash\n",
        }]);
        assert_ne!(before, after);
    }
}
