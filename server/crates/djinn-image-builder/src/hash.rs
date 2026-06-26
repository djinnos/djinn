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
    build_version: &str,
) -> String {
    let script_sha = compute_script_bundle_sha();
    let config_json = canonical_json(config);

    let mut hasher = Sha256::new();
    // v2→v3: the v2 attempt moved RUSTUP_HOME to /cache/rustup, but
    // /cache is a runtime PVC mount that overlays whatever the image
    // baked at /cache — so install-rust.sh wrote to a layer that was
    // hidden by the empty PVC at startup, leaving workers with no
    // cargo/rustup at all. v3 keeps RUSTUP_HOME at the baked-in
    // /usr/local/rustup and makes it world-writable via emit_cleanup
    // so workspace-pinned toolchains (rust-toolchain.toml → 1.94.1)
    // can still be installed at session time without spilling into
    // the workspace.
    //
    // v3→v4: bake `clippy` + `rustfmt` into the Rust toolchain components
    // (RUST_COMPONENTS in dockerfile.rs) so the task-run gate
    // (`cargo clippy -- -D warnings`, `cargo fmt --check`) doesn't trigger a
    // session-time `rustup component add` against the read-only RUSTUP_HOME
    // ("cannot write to /usr/local/rustup/tmp"). RUST_COMPONENTS is a hardcoded
    // const (not part of `config_json`/`script_sha`), so this salt bump is what
    // forces every cached project image to rebuild with the new components.
    //
    // v4→v5: route Go's caches to the /cache PVC (`GOMODCACHE=/cache/go/mod`,
    // `GOCACHE=/cache/go/build` in emit_path) so `go mod download`/`go test`
    // under the Landlock sandbox can write them — the old defaults (/go/pkg/mod
    // root-owned + not in the sandbox allowlist; $HOME/.cache/go-build outside
    // it) failed with "cannot write /go/pkg/mod". Those ENV lines are hardcoded
    // in dockerfile.rs (not in config_json/script_sha), so the salt bump is what
    // rebuilds every cached project image with the new Go cache paths.
    //
    // v5→v6: set `HOME=/home/djinn` (emit_path) + create that user/home in
    // base-debian.sh + bake gopls into /go/bin (install-go.sh). Pods run as
    // runAsUser=10001 with no home, so HOME was "/" → the agent's scratch dir,
    // the LSP install dir, and Go's GOCACHE all hit EACCES under root-owned /.
    // The two script edits already move script_sha, but the HOME ENV line is a
    // hardcoded const here too, so bump the salt to document + guarantee the
    // rebuild.
    //
    // v6→v7: verification left `EnvironmentConfig`. Removing the field already
    // changes `config_json` for every existing row, but bump the salt to
    // document the decoupling and guarantee the one-time rebuild. The hash is
    // now build-only: it covers languages, workspaces, system_packages, env,
    // lifecycle, and the script/worker refs.
    //
    // v7→v8: install-rust.sh's /etc/profile.d/10-rust.sh fragment now uses
    // `${CARGO_HOME:-/usr/local/cargo}` instead of an unconditional export.
    // The agent's shell tool runs `bash -lc` (login), so the old unconditional
    // export clobbered the pod-level CARGO_HOME=/cache/cargo back to the
    // ephemeral image layer → agent-invoked cargo re-downloaded crates cold.
    // The script edit already moves script_sha; bump the salt to document.
    //
    // v8→v9: v8 fixed the cache clobber but, by keeping the PATH line deriving
    // from `${CARGO_HOME}/bin`, pushed a NEW bug: at runtime CARGO_HOME is
    // /cache/cargo (the PVC cache dir, no cargo binary), so the login-shell PATH
    // got /cache/cargo/bin and `cargo` fell off PATH — the agent dropped to
    // cold, uncached `rustc` fallbacks (~12-min compiles) and `cargo: command
    // not found` loops. The 10-rust.sh fragment now pins PATH to the baked
    // /usr/local/cargo/bin (expanded at build time) while CARGO_HOME stays
    // runtime-overridable for the cache. Script edit moves script_sha; bump the
    // salt to document + guarantee the rebuild.
    //
    // v9→v10: fold the djinn release `build_version` into the hash. The agent
    // worker binary (prompts via `djinn-roles` + tool schemas via
    // `djinn-mcp-extension`) is `COPY --from`'d into the catalog image, so a
    // change to a prompt or tool must rebuild every project image. Keying only
    // on `agent_worker_ref` missed this whenever that ref was an unversioned
    // tag (`:latest`) or a reused tag — leaving task-run pods on stale agent
    // code after a deploy. Mixing in the version guarantees a rebuild on bump.
    hasher.update(b"env-config/v10\0");
    hasher.update(config_json.as_bytes());
    hasher.update([0u8]);
    hasher.update(script_sha.as_bytes());
    hasher.update([0u8]);
    hasher.update(agent_worker_ref.as_bytes());
    hasher.update([0u8]);
    hasher.update(build_version.as_bytes());
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
        let a = compute_environment_hash(&c, "djinn/agent-worker:sha-abc", "0.6.57");
        let b = compute_environment_hash(&c, "djinn/agent-worker:sha-abc", "0.6.57");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn hash_changes_when_config_changes() {
        let mut c = cfg();
        let a = compute_environment_hash(&c, "djinn/agent-worker:sha-abc", "0.6.57");
        c.env.insert("RUST_LOG".into(), "info".into());
        let b = compute_environment_hash(&c, "djinn/agent-worker:sha-abc", "0.6.57");
        assert_ne!(a, b);
    }

    #[test]
    fn hash_changes_when_worker_ref_changes() {
        let c = cfg();
        let a = compute_environment_hash(&c, "djinn/agent-worker:sha-abc", "0.6.57");
        let b = compute_environment_hash(&c, "djinn/agent-worker:sha-def", "0.6.57");
        assert_ne!(a, b);
    }

    #[test]
    fn hash_changes_when_build_version_changes() {
        // A version bump must invalidate every catalog image so new agent
        // prompts/tools propagate — even when the worker-image tag is unchanged
        // (e.g. `:latest`).
        let c = cfg();
        let a = compute_environment_hash(&c, "djinn/agent-runtime:latest", "0.6.56");
        let b = compute_environment_hash(&c, "djinn/agent-runtime:latest", "0.6.57");
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
