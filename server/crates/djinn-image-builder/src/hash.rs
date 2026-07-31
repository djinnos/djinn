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
//! 4. The declared launcher authority protocol, whenever it is not the
//!    default. See [`compute_environment_hash`] for why the default is
//!    excluded rather than mixed in unconditionally.
//!
//! The hash is sha256 of the concatenated inputs, lowercase hex. It's
//! intentionally 64 chars so log output is readable (cf. the image tag
//! format `djinn-project-<id>:<hash>`).

use djinn_launcher_protocol::LauncherAuthorityProtocol;
use sha2::{Digest, Sha256};

use djinn_stack::environment::EnvironmentConfig;

use crate::dockerfile::DEFAULT_LAUNCHER_PROTOCOL;
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
///
/// # The launcher authority protocol is an input
///
/// `launcher_protocol` is the declaration the artifact will carry. Mixing it in
/// is what makes a protocol change *unable* to resolve to a cached image: the
/// image tag is `djinn-project-<id>:<hash-prefix>`, so `leaf-v1` and
/// `resize-v2` builds of an otherwise identical config live at different tags,
/// and a flip re-triggers a build instead of leaving the old launcher in place
/// under a catalog row claiming the new protocol.
///
/// Prefer [`crate::BuildContext::environment_hash`], which passes the
/// declaration that was actually emitted into the Dockerfile.
///
/// # Why the default is excluded from the pre-image
///
/// [`DEFAULT_LAUNCHER_PROTOCOL`] contributes *nothing*, so the pre-image for a
/// deployment that configures no protocol is byte-identical to the pre-image
/// before the protocol became configurable. Mixing it in unconditionally would
/// change every hash in every existing deployment and force a fleet-wide
/// rebuild on upgrade for a change that alters no image. Only a deviation from
/// the default perturbs the hash — which is exactly the case that must not
/// reuse a cached artifact. `the_default_protocol_hashes_to_the_pre_change_preimage`
/// pins this.
pub fn compute_environment_hash(
    config: &EnvironmentConfig,
    agent_worker_ref: &str,
    build_version: &str,
    launcher_protocol: LauncherAuthorityProtocol,
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
    // v10→v11: `/home/djinn` is group-owned by the artifact GID 1000 with setgid
    // 2775 in base-debian.sh instead of `10001:10001 0775`. Since `qut0` the
    // task-run/warm Pod runs as uid/gid 1000, so it matched "other" (r-x) on its
    // own $HOME and could not create anything there — every worker and planner
    // session died on `create durable blobs: Permission denied` resolving
    // `$HOME/.cache/djinn/output_stash`, and fnm/gopls/npm/`git config --global`
    // sat on the same wall (9jrg). The worker now fails readiness loudly on an
    // unwritable $HOME (`volume_contract::check_home_writable`), which every
    // cached pre-fix image would trip, so this rebuild is a hard prerequisite,
    // not an optimization. The script edit already moves script_sha; bump the
    // salt to document it and guarantee the rebuild.
    hasher.update(b"env-config/v11\0");
    hasher.update(config_json.as_bytes());
    hasher.update([0u8]);
    hasher.update(script_sha.as_bytes());
    hasher.update([0u8]);
    hasher.update(agent_worker_ref.as_bytes());
    hasher.update([0u8]);
    hasher.update(build_version.as_bytes());
    if launcher_protocol != DEFAULT_LAUNCHER_PROTOCOL {
        hasher.update([0u8]);
        hasher.update(b"launcher-authority-protocol\0");
        hasher.update(launcher_protocol.as_wire().as_bytes());
    }
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
        let a = compute_environment_hash(
            &c,
            "djinn/agent-worker:sha-abc",
            "0.6.57",
            DEFAULT_LAUNCHER_PROTOCOL,
        );
        let b = compute_environment_hash(
            &c,
            "djinn/agent-worker:sha-abc",
            "0.6.57",
            DEFAULT_LAUNCHER_PROTOCOL,
        );
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn hash_changes_when_config_changes() {
        let mut c = cfg();
        let a = compute_environment_hash(
            &c,
            "djinn/agent-worker:sha-abc",
            "0.6.57",
            DEFAULT_LAUNCHER_PROTOCOL,
        );
        c.env.insert("RUST_LOG".into(), "info".into());
        let b = compute_environment_hash(
            &c,
            "djinn/agent-worker:sha-abc",
            "0.6.57",
            DEFAULT_LAUNCHER_PROTOCOL,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn hash_changes_when_worker_ref_changes() {
        let c = cfg();
        let a = compute_environment_hash(
            &c,
            "djinn/agent-worker:sha-abc",
            "0.6.57",
            DEFAULT_LAUNCHER_PROTOCOL,
        );
        let b = compute_environment_hash(
            &c,
            "djinn/agent-worker:sha-def",
            "0.6.57",
            DEFAULT_LAUNCHER_PROTOCOL,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn hash_changes_when_build_version_changes() {
        // A version bump must invalidate every catalog image so new agent
        // prompts/tools propagate — even when the worker-image tag is unchanged
        // (e.g. `:latest`).
        let c = cfg();
        let a = compute_environment_hash(
            &c,
            "djinn/agent-runtime:latest",
            "0.6.56",
            DEFAULT_LAUNCHER_PROTOCOL,
        );
        let b = compute_environment_hash(
            &c,
            "djinn/agent-runtime:latest",
            "0.6.57",
            DEFAULT_LAUNCHER_PROTOCOL,
        );
        assert_ne!(a, b);
    }

    /// **A protocol change cannot resolve to a cached image.** The controller
    /// skips the build when the stored `config_hash` still equals the computed
    /// one, and derives the image tag from that hash — so if the declaration
    /// did not move the hash, flipping a deployment to `resize-v2` would leave
    /// every project on the artifact its old launcher built, with the catalog
    /// eventually claiming the new protocol over it.
    ///
    /// MUTATION: drop the `launcher_protocol` block from
    /// `compute_environment_hash` (or hash `DEFAULT_LAUNCHER_PROTOCOL` in both
    /// arms). Both `assert_ne!`s below fail.
    #[test]
    fn hash_changes_when_the_declared_protocol_changes() {
        let c = cfg();
        let leaf = compute_environment_hash(
            &c,
            "djinn/agent-worker:sha-abc",
            "0.6.57",
            LauncherAuthorityProtocol::LeafV1,
        );
        let resize = compute_environment_hash(
            &c,
            "djinn/agent-worker:sha-abc",
            "0.6.57",
            LauncherAuthorityProtocol::ResizeV2,
        );
        assert_ne!(
            leaf, resize,
            "a protocol flip must invalidate the cached image; identical hashes mean the \
             controller never rebuilds and the tag never moves"
        );

        // …and the same must hold for every pair the type admits, so adding a
        // third protocol cannot quietly collide with an existing one.
        let mut seen = std::collections::BTreeSet::new();
        for protocol in LauncherAuthorityProtocol::ALL {
            let hash =
                compute_environment_hash(&c, "djinn/agent-worker:sha-abc", "0.6.57", protocol);
            assert!(
                seen.insert(hash),
                "{protocol} shares an environment hash with another protocol"
            );
        }
    }

    /// **The default is a no-op for every existing deployment.** Reconstructs
    /// the pre-image `compute_environment_hash` had *before* the protocol
    /// became an input, from this test's own bytes, and asserts the default
    /// still hashes to exactly that. If it did not, upgrading a deployment that
    /// configures nothing would invalidate every catalog image and rebuild the
    /// whole fleet for a change that alters no artifact.
    ///
    /// MUTATION: hash the protocol unconditionally (delete the `if
    /// launcher_protocol != DEFAULT_LAUNCHER_PROTOCOL` guard). This fails.
    /// A deliberate salt bump also fails it, which is correct: the pre-image is
    /// exactly what a salt bump changes, so it must be restated here on purpose.
    #[test]
    fn the_default_protocol_hashes_to_the_pre_change_preimage() {
        let c = cfg();
        let worker = "djinn/agent-worker:sha-abc";
        let version = "0.6.57";

        let mut legacy = Sha256::new();
        legacy.update(b"env-config/v11\0");
        legacy.update(canonical_json(&c).as_bytes());
        legacy.update([0u8]);
        legacy.update(compute_script_bundle_sha().as_bytes());
        legacy.update([0u8]);
        legacy.update(worker.as_bytes());
        legacy.update([0u8]);
        legacy.update(version.as_bytes());

        assert_eq!(
            compute_environment_hash(&c, worker, version, DEFAULT_LAUNCHER_PROTOCOL),
            hex_lower(&legacy.finalize()),
            "configuring no protocol must not perturb the hash of a single existing image"
        );
        assert_eq!(DEFAULT_LAUNCHER_PROTOCOL, LauncherAuthorityProtocol::LeafV1);
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
