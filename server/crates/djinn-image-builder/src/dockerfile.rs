//! Generate a Dockerfile from an [`EnvironmentConfig`].
//!
//! Pure function. The output is a tuple of (Dockerfile string, script
//! bundle) — the builder Pod drops the Dockerfile + bundle into a
//! ConfigMap and runs `buildctl build` against it.
//!
//! Structure of the emitted Dockerfile:
//!
//! ```text
//! FROM <base>
//! COPY scripts/ /tmp/djinn-scripts/
//! RUN /tmp/djinn-scripts/base-<distro>.sh
//! RUN APT_PACKAGES="..." /tmp/djinn-scripts/install-system.sh
//! COPY --from=djinn/agent-worker:<sha> /opt/djinn/bin/djinn-agent-worker /opt/djinn/bin/djinn-agent-worker
//! RUN /tmp/djinn-scripts/install-agent-worker.sh
//! RUN TOOLCHAINS="stable 1.85.0" COMPONENTS="rust-analyzer" /tmp/djinn-scripts/install-rust.sh
//! RUN NODE_VERSIONS="20 22" PACKAGE_MANAGERS="pnpm" /tmp/djinn-scripts/install-node.sh
//! ENV RUST_LOG=info
//! RUN <post_build hook>
//! RUN rm -rf /tmp/djinn-scripts
//! ```
//!
//! Language blocks are emitted only when the corresponding
//! `config.languages.*` is `Some`. Workspace-level toolchain overrides
//! are aggregated into the space-separated env vars each installer
//! consumes (e.g. `TOOLCHAINS="stable 1.85.0"`), so one `RUN` line per
//! language covers every pinned version.

use std::collections::BTreeSet;
use std::fmt::Write;

use djinn_launcher_protocol::LauncherAuthorityProtocol;
use djinn_stack::environment::{
    EnvironmentConfig, HookCommand, Languages, NodeLanguage, RustLanguage,
};
use thiserror::Error;

use crate::scripts::SCRIPTS;

/// The launcher authority protocol an image declares when the deployment
/// selects nothing.
///
/// The declaration is a property of the **artifact**, not of its name: the
/// `djinn-cgroup-launcher` binary [`emit_agent_worker`] copies in writes its
/// own invocation-leaf `cpu.max`, and that is exactly what
/// [`LauncherAuthorityProtocol::LeafV1`] means.
///
/// Which protocol a given build declares is an **input**:
/// [`generate_dockerfile`] takes it, the deployment supplies it (see
/// `djinn_image_controller::ImageControllerConfig::declared_launcher_protocol`),
/// and this constant is only the fallback. That is what makes a resize cutover
/// reachable at all — a hardcoded declaration cannot be rolled out.
///
/// # Why flipping it no longer needs a manual salt bump
///
/// The selected protocol is an input to [`crate::compute_environment_hash`], so
/// a deployment that flips to `resize-v2` necessarily computes a different
/// environment hash, lands on a different content-addressed tag, and cannot
/// reuse the cached `leaf-v1` artifact. The historical hazard — "a cached image
/// keeps its old launcher while the catalog claims the new protocol" — is now
/// closed by construction instead of by a comment asking for a manual bump.
/// [`BuildContext::verify_declaration`] closes the other half: the build Job
/// that reports the declaration to the catalog refuses to render unless the
/// Dockerfile it is about to build declares the same thing.
pub const DEFAULT_LAUNCHER_PROTOCOL: LauncherAuthorityProtocol = LauncherAuthorityProtocol::LeafV1;

/// OCI label key carrying the declaration into the built artifact's metadata.
///
/// This — not the image tag — is where the protocol is declared. A tag is
/// mutable naming that can be made to say anything; build metadata is written
/// once, at artifact creation, by the thing that actually assembled the image.
pub const LAUNCHER_PROTOCOL_LABEL: &str = "djinn.app/launcher-authority-protocol";

/// Environment variable the launcher sidecar reads out of the same image at
/// runtime. Carries the identical wire string as [`LAUNCHER_PROTOCOL_LABEL`].
pub const LAUNCHER_PROTOCOL_ENV: &str = "DJINN_LAUNCHER_AUTHORITY_PROTOCOL";

/// The agent-worker helper-image ref baked into the `COPY --from=...`
/// line. The tag is the sha of the worker binary used at build time.
/// Plumbed in from the caller so the Dockerfile string becomes
/// deterministic for a given input; `compute_environment_hash` mixes
/// it in, so a worker rebuild invalidates cached project images.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentWorkerImage {
    /// e.g. `djinn/agent-worker` — no tag, no sha.
    pub repository: String,
    /// e.g. `sha256-abc123…` or a semver tag; must be non-empty.
    pub reference: String,
}

impl AgentWorkerImage {
    pub fn new(repository: impl Into<String>, reference: impl Into<String>) -> Self {
        Self {
            repository: repository.into(),
            reference: reference.into(),
        }
    }

    fn full_ref(&self) -> String {
        format!("{}:{}", self.repository, self.reference)
    }
}

/// The output of [`generate_dockerfile`] — the Dockerfile string plus
/// the script bundle that needs to land alongside it in the builder
/// context.
#[derive(Debug, Clone)]
pub struct BuildContext {
    pub dockerfile: String,
    /// Each entry is `(relative_path_in_context, body)`. The generator
    /// always returns the full [`crate::scripts::SCRIPTS`] bundle so
    /// that every installer a project *might* reference is available —
    /// the alternative (per-config filtering) would leak into the hash
    /// as the config evolves.
    pub scripts: Vec<(String, String)>,
    /// The launcher authority protocol the emitted Dockerfile declares, so the
    /// build Job can echo the *same* value into its build metadata without
    /// re-deriving it from anything (least of all the image tag).
    pub launcher_protocol: LauncherAuthorityProtocol,
}

impl BuildContext {
    /// Check that [`Self::dockerfile`] declares exactly [`Self::launcher_protocol`].
    ///
    /// The catalog row is written from the build Job's sentinel, and that
    /// sentinel is rendered from [`Self::launcher_protocol`] — while the
    /// artifact's own OCI label comes from the Dockerfile text. If the two ever
    /// disagreed, the catalog would claim one authority while the shipped
    /// launcher implemented the other: two owners of the same Pod's CPU quota,
    /// or none. [`generate_dockerfile`] writes both from the same argument, and
    /// this is what forbids anything downstream from editing one of them —
    /// `djinn_image_controller::build_job::build_image_build_job` calls it and
    /// refuses to render a Job when it fails, so a disagreeing context cannot
    /// reach a build at all.
    ///
    /// Returns the agreed protocol so callers can use the checked value rather
    /// than re-reading the field they just validated.
    pub fn verify_declaration(&self) -> Result<LauncherAuthorityProtocol, DeclarationError> {
        let label = single_directive_value(&self.dockerfile, "LABEL", LAUNCHER_PROTOCOL_LABEL);
        let env = single_directive_value(&self.dockerfile, "ENV", LAUNCHER_PROTOCOL_ENV);

        let (Some(label), Some(env)) = (label, env) else {
            return Err(DeclarationError::Undeclared {
                context_says: self.launcher_protocol,
            });
        };
        if label != env {
            return Err(DeclarationError::LabelEnvDisagree {
                label_says: label,
                env_says: env,
            });
        }
        let declared = label.parse::<LauncherAuthorityProtocol>().map_err(|_| {
            DeclarationError::Unrecognized {
                dockerfile_says: label.clone(),
            }
        })?;
        if declared != self.launcher_protocol {
            return Err(DeclarationError::Disagree {
                dockerfile_says: declared,
                context_says: self.launcher_protocol,
            });
        }
        Ok(declared)
    }

    /// The environment hash this context must be cached under.
    ///
    /// Deliberately a method on the rendered context rather than a free
    /// function taking a protocol: the hash is then computed from the
    /// declaration that was *actually emitted*, so a caller cannot hash under
    /// `leaf-v1` and build an artifact that declares `resize-v2`. The tag is
    /// derived from this hash, so the two protocols can never share one tag and
    /// a protocol change can never resolve to a cached image.
    pub fn environment_hash(
        &self,
        config: &EnvironmentConfig,
        agent_worker_ref: &str,
        build_version: &str,
    ) -> String {
        crate::hash::compute_environment_hash(
            config,
            agent_worker_ref,
            build_version,
            self.launcher_protocol,
        )
    }
}

/// Read the single value of a `DIRECTIVE key=value` line, or `None` when the
/// key appears zero or more than one time. "More than once" is a failure, not a
/// last-one-wins: a second declaration is exactly how an artifact ends up
/// saying two different things.
fn single_directive_value(dockerfile: &str, directive: &str, key: &str) -> Option<String> {
    let prefix = format!("{directive} {key}=");
    let mut found: Option<String> = None;
    for line in dockerfile.lines() {
        let Some(value) = line.trim_end().strip_prefix(prefix.as_str()) else {
            continue;
        };
        if found.is_some() {
            return None;
        }
        found = Some(value.trim_matches('"').to_string());
    }
    found
}

/// The artifact's declaration and the context reporting it to the catalog do
/// not agree. Always fatal: there is no safe way to guess which of two
/// authorities owns a Pod's CPU quota.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DeclarationError {
    #[error(
        "the generated Dockerfile declares no launcher authority protocol, but the build context \
         reports `{context_says}` — the catalog would record an authority the artifact never claims"
    )]
    Undeclared {
        context_says: LauncherAuthorityProtocol,
    },
    #[error(
        "the artifact's LABEL declares `{label_says}` but its ENV declares `{env_says}` — the \
         image and the launcher sidecar reading it would disagree at runtime"
    )]
    LabelEnvDisagree {
        label_says: String,
        env_says: String,
    },
    #[error(
        "the generated Dockerfile declares `{dockerfile_says}`, which is not a launcher \
             authority protocol this plane knows"
    )]
    Unrecognized { dockerfile_says: String },
    #[error(
        "the generated Dockerfile declares `{dockerfile_says}` but the build context reports \
         `{context_says}` to the catalog — refusing to build an artifact whose label and catalog \
         row disagree about which component owns CPU quota"
    )]
    Disagree {
        dockerfile_says: LauncherAuthorityProtocol,
        context_says: LauncherAuthorityProtocol,
    },
}

#[derive(Debug, Error)]
pub enum DockerfileError {
    #[error(
        "config.source = user-edited but schema_version = 0; refuse to build from an un-seeded row"
    )]
    UnseededConfig,
    #[error("config validation failed: {0}")]
    InvalidConfig(#[from] djinn_stack::environment::EnvironmentConfigError),
    #[error("agent-worker reference is empty — caller must plumb a real tag or sha")]
    MissingAgentWorkerRef,
    #[error("unknown language slug in workspace: {0}")]
    UnknownWorkspaceLanguage(String),
}

/// Generate the Dockerfile + script bundle for `config`.
///
/// `agent_worker` is the helper image the `djinn-agent-worker` binary
/// is copied from. The worker reference contributes to
/// [`crate::compute_environment_hash`], so replacing it (e.g. after a
/// worker rebuild) invalidates cached images.
///
/// `launcher_protocol` is the declaration this artifact will carry — supplied
/// by the deployment, defaulting to [`DEFAULT_LAUNCHER_PROTOCOL`]. It is a
/// required argument rather than a defaulted one on purpose: a caller that can
/// silently omit it is a caller that can silently keep a fleet on `leaf-v1`
/// after an operator configured the cutover. It is written into the Dockerfile
/// and reported on [`BuildContext::launcher_protocol`] from this one value, and
/// [`BuildContext::environment_hash`] hashes the same value, so the
/// declaration, the artifact, and the cache key cannot come apart.
pub fn generate_dockerfile(
    config: &EnvironmentConfig,
    agent_worker: &AgentWorkerImage,
    launcher_protocol: LauncherAuthorityProtocol,
) -> Result<BuildContext, DockerfileError> {
    if agent_worker.reference.trim().is_empty() {
        return Err(DockerfileError::MissingAgentWorkerRef);
    }
    config.validate()?;

    let mut df = String::new();
    writeln!(df, "# syntax=docker/dockerfile:1.7").unwrap();
    writeln!(df, "# Generated by djinn-image-builder — DO NOT EDIT.").unwrap();
    writeln!(df).unwrap();

    emit_from(&mut df, config);
    emit_copy_scripts(&mut df);
    emit_base(&mut df, config);
    emit_path(&mut df);
    emit_system_packages(&mut df, config);
    emit_agent_worker(&mut df, agent_worker);
    emit_launcher_protocol(&mut df, launcher_protocol);
    emit_language_blocks(&mut df, config)?;
    emit_env(&mut df, config);
    emit_post_build_hooks(&mut df, config);
    emit_cleanup(&mut df);

    Ok(BuildContext {
        dockerfile: df,
        scripts: SCRIPTS
            .iter()
            .map(|s| (format!("scripts/{}", s.name), s.body.to_string()))
            .collect(),
        launcher_protocol,
    })
}

// ---- section emitters ---------------------------------------------------

// Base image must match the glibc of `djinn-agent-runtime`
// (debian:trixie-slim, see server/docker/djinn-agent-runtime-base.Dockerfile)
// because the `djinn-agent-worker` binary is COPY'd out of that runtime
// image and executed *inside* this devcontainer image (warm-graph jobs +
// task runs). The worker links GLIBC 2.39 / GLIBCXX 3.4.32 from trixie;
// on the older bookworm base (glibc 2.36) it dies at startup with
// `version 'GLIBC_2.38' not found`. The 2026-04-22 cleanup comment that
// claimed "libc flavor is irrelevant" predated the in-image worker — it
// is very much relevant now, so keep this pinned to trixie.
const BASE_IMAGE: &str = "debian:trixie-slim";
const BASE_SETUP_SCRIPT: &str = "base-debian.sh";

fn emit_from(df: &mut String, _config: &EnvironmentConfig) {
    writeln!(df, "FROM {BASE_IMAGE}").unwrap();
}

fn emit_copy_scripts(df: &mut String) {
    writeln!(df, "COPY scripts/ /tmp/djinn-scripts/").unwrap();
    writeln!(df, "RUN chmod -R 0755 /tmp/djinn-scripts").unwrap();
}

fn emit_base(df: &mut String, _config: &EnvironmentConfig) {
    writeln!(df, "RUN /tmp/djinn-scripts/{BASE_SETUP_SCRIPT}").unwrap();
}

// Canonical PATH + language-toolchain env for the generated image.
//
// install-rust.sh / install-node.sh / ... drop shell fragments in
// /etc/profile.d that set PATH + RUSTUP_HOME + CARGO_HOME, but those
// fragments only fire for LOGIN shells — the agent-worker spawns
// subprocesses (cargo, rust-analyzer, pnpm) via `Command::new`, which
// inherits the image-level ENV. Without these lines `rust-analyzer`
// is on PATH as a rustup shim, but rustup looks at $HOME/.rustup
// (empty), reports "no installed toolchains", and fails the SCIP step.
//
// Paths and vars that don't exist at runtime are harmless — PATH skips
// over missing dirs, rustup ignores RUSTUP_HOME if Rust isn't installed.
// Listing everything unconditionally keeps the emitter simple and the
// generated hash stable across language toggles.
// `/cache/go/bin` is the runtime GOBIN (see GOBIN note in emit_path). It is
// listed LAST on purpose: tools the agent `go install`s land on the persistent
// per-project /cache PVC, so keeping that dir lowest-priority means a binary
// left there by a prior task-run can never shadow a baked toolchain or system
// binary earlier on PATH.
const IMAGE_PATH: &str = "/opt/djinn/bin:/usr/local/cargo/bin:/opt/node/bin:/usr/local/go/bin:/go/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/cache/go/bin";

fn emit_path(df: &mut String) {
    writeln!(df, "ENV PATH={IMAGE_PATH}").unwrap();
    // HOME must be an explicit image ENV: the Pod runs as runAsUser=10001 with
    // no logged-in shell, so without this the agent's spawned subprocesses
    // inherit HOME="/" (root-owned) and every $HOME-relative path fails with
    // EACCES — the Landlock scratch dir (~/.cache/djinn), the LSP auto-install
    // dir (~/.local/share/djinn/bin), Go's default GOCACHE. base-debian.sh
    // creates the matching djinn user + writable /home/djinn.
    writeln!(df, "ENV HOME=/home/djinn").unwrap();
    // Keep RUSTUP_HOME at the baked-in /usr/local/rustup so the
    // build-time install-rust.sh writes there (the rustup install is in
    // the image layer, not a PVC mount — /cache IS a PVC mount at
    // runtime, so putting RUSTUP_HOME there would hide the toolchain
    // entirely behind an empty PVC overlay).
    //
    // Workspace toolchains pinned in rust-toolchain.toml (e.g. 1.94.1)
    // need a writable RUSTUP_HOME, which we make possible by chmod-ing
    // the directory world-writable in the cleanup pass — see
    // `emit_cleanup`. That keeps stable readable to everyone AND lets
    // the non-root djinn user `rustup install <pinned>` at session
    // time without falling back to spilling .rustup/ into the workspace.
    writeln!(
        df,
        "ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo"
    )
    .unwrap();
    // GOPATH stays /go (image layer): install-go.sh `go install`s scip-go to
    // /go/bin at build time and IMAGE_PATH lists /go/bin, so the indexer must
    // not be hidden behind the empty /cache PVC overlay — same constraint as
    // RUSTUP_HOME above.
    //
    // But Go's caches must be WRITABLE at runtime, and the agent runs build/
    // test via the Landlock-sandboxed `shell` tool. /go is root-owned and not
    // in the Landlock allowlist, so leaving the module cache at its default
    // ($GOPATH/pkg/mod = /go/pkg/mod) fails with "cannot write /go/pkg/mod";
    // likewise GOCACHE's default ($HOME/.cache/go-build) is unix-writable but
    // outside the sandbox allowlist. Point both at the /cache PVC, which is
    // djinn-owned (EFS access point uid 10001), persistent across task-runs,
    // and explicitly write-allowed in the sandbox (see djinn-agent
    // sandbox/linux.rs). go creates these dirs at runtime on the writable PVC.
    // GOBIN follows the same logic: its default ($GOPATH/bin = /go/bin) is
    // root-owned and outside the sandbox allowlist, so a runtime `go install`
    // (e.g. the agent pulling a CLI tool, or a repo's `go install` codegen step)
    // fails with "cannot write /go/bin". Point it at the writable /cache PVC so
    // installed binaries land somewhere the non-root djinn user can write and
    // that IMAGE_PATH already lists. Build-time installs that must stay in the
    // image layer (scip-go) set GOBIN=/go/bin explicitly on the command line, so
    // this runtime default does not divert them onto the PVC-shadowed path.
    writeln!(df, "ENV GOPATH=/go GOROOT=/usr/local/go").unwrap();
    writeln!(
        df,
        "ENV GOMODCACHE=/cache/go/mod GOCACHE=/cache/go/build GOBIN=/cache/go/bin"
    )
    .unwrap();
    // Node + Python dependency caches — same rationale as GOMODCACHE/GOCACHE
    // above, applied to pnpm/npm/yarn (node) and pip/uv (python). Left at their
    // defaults these stores land under $HOME (pnpm store → $XDG_DATA_HOME/pnpm/
    // store i.e. ~/.local/share/pnpm/store; npm → ~/.npm; yarn → ~/.cache/yarn;
    // pip → ~/.cache/pip; uv → ~/.cache/uv), which are (1) ephemeral home paths
    // lost when the Pod dies, so every task-run re-downloads the whole
    // dependency closure, and (2) OUTSIDE the Landlock sandbox writable
    // allowlist (only /cache and ~/.cache/djinn are writable — see
    // djinn-sandbox/src/linux.rs), so a sandboxed `pnpm install` / `pip install`
    // is denied and fails. Point them at the djinn-owned, persistent,
    // sandbox-writable /cache PVC — the same volume CARGO_HOME/GOMODCACHE use.
    // These stores are content-addressed by package@version (like the Cargo
    // registry and the Go module cache), so a SINGLE shared store across
    // projects is correct and de-dupes common packages — no per-project
    // namespacing (unlike SCCACHE_DIR / CARGO_TARGET_DIR, which hold
    // workspace-specific compiled artifacts). PNPM_HOME governs pnpm's global
    // dir AND its store, whose default is $PNPM_HOME/store. As with the Cargo/Go
    // ENV above these are unconditional and harmless when the language isn't
    // installed. Baked as image ENV (not per-language RUN blocks) so warm Pods
    // and task-run Pods inherit identical values from the image, with no runtime
    // override in djinn-k8s/src/job.rs.
    writeln!(
        df,
        "ENV PNPM_HOME=/cache/pnpm npm_config_cache=/cache/npm YARN_CACHE_FOLDER=/cache/yarn"
    )
    .unwrap();
    writeln!(df, "ENV PIP_CACHE_DIR=/cache/pip UV_CACHE_DIR=/cache/uv").unwrap();
}

fn emit_system_packages(df: &mut String, config: &EnvironmentConfig) {
    let apt = space_join_sorted(&config.system_packages);
    if apt.is_empty() {
        return;
    }
    writeln!(
        df,
        "RUN APT_PACKAGES=\"{apt}\" /tmp/djinn-scripts/install-system.sh"
    )
    .unwrap();
}

fn emit_agent_worker(df: &mut String, agent_worker: &AgentWorkerImage) {
    // The agent-runtime image ships the binary at /usr/local/bin/
    // (matches its own ENTRYPOINT). We copy it into the project image
    // at /opt/djinn/bin/ so our PATH-prefix install script can guarantee
    // it wins over user-installed tools with overlapping names.
    writeln!(
        df,
        "COPY --from={} /usr/local/bin/djinn-agent-worker /opt/djinn/bin/djinn-agent-worker",
        agent_worker.full_ref()
    )
    .unwrap();
    // The mandatory cgroup-launcher sidecar runs from THIS image with a
    // different entrypoint (`/opt/djinn/bin/djinn-cgroup-launcher`, rendered by
    // djinn-k8s::launcher). Copy it from the same agent-worker stage so the
    // launcher command resolves to a real packaged artifact — no separate image.
    writeln!(
        df,
        "COPY --from={} /usr/local/bin/djinn-cgroup-launcher /opt/djinn/bin/djinn-cgroup-launcher",
        agent_worker.full_ref()
    )
    .unwrap();
    writeln!(df, "RUN /tmp/djinn-scripts/install-agent-worker.sh").unwrap();
}

/// Declare, in the artifact's own build metadata, which component owns the CPU
/// quota of the invocation leaves the launcher copied in above will create.
///
/// Emitted immediately after that `COPY`, because it describes exactly that
/// binary. Both lines carry [`LauncherAuthorityProtocol::as_wire`] verbatim —
/// the same string migration 164 CHECKs and `djinn-db` persists — so the
/// declaration cannot drift from the vocabulary the rest of the plane agrees
/// on.
fn emit_launcher_protocol(df: &mut String, protocol: LauncherAuthorityProtocol) {
    writeln!(
        df,
        "LABEL {LAUNCHER_PROTOCOL_LABEL}=\"{}\"",
        protocol.as_wire()
    )
    .unwrap();
    writeln!(df, "ENV {LAUNCHER_PROTOCOL_ENV}={}", protocol.as_wire()).unwrap();
}

fn emit_language_blocks(
    df: &mut String,
    config: &EnvironmentConfig,
) -> Result<(), DockerfileError> {
    emit_rust_block(df, &config.languages, config);
    emit_node_block(df, &config.languages, config);
    emit_python_block(df, &config.languages, config);
    emit_go_block(df, &config.languages, config);
    emit_java_block(df, &config.languages, config);
    emit_ruby_block(df, &config.languages, config);
    emit_dotnet_block(df, &config.languages);
    emit_clang_block(df, &config.languages);

    // Catch misspellings — if a workspace declares a language no
    // installer covers, fail the build rather than silently dropping it.
    let known = [
        "rust", "node", "python", "go", "java", "ruby", "dotnet", "clang",
    ];
    for ws in &config.workspaces {
        if !known.contains(&ws.language.as_str()) {
            return Err(DockerfileError::UnknownWorkspaceLanguage(
                ws.language.clone(),
            ));
        }
    }
    Ok(())
}

// `rust-analyzer` is mandatory (the warm-graph SCIP indexer calls it).
// `clippy` + `rustfmt` are baked in for the task-run gate (`cargo clippy
// -- -D warnings`, `cargo fmt --check`): without them rustup would try to
// `component add` them at SESSION time for the repo's pinned toolchain, which
// writes to the (non-root-owned) RUSTUP_HOME and fails with
// "cannot write to /usr/local/rustup/tmp". install-rust.sh loops COMPONENTS
// over every pinned toolchain, so they're present for the exact
// rust-toolchain.toml version. `targets` was dropped in the 2026-04-22
// cleanup — djinn's workflow (clippy/check/test against the host target)
// never needs cross-targets.
const RUST_COMPONENTS: &str = "rust-analyzer clippy rustfmt";

fn emit_rust_block(df: &mut String, languages: &Languages, config: &EnvironmentConfig) {
    let Some(rust) = &languages.rust else { return };
    let toolchains = aggregate_rust_toolchains(rust, config);
    let line = format!(
        "RUN TOOLCHAINS=\"{}\" DEFAULT_TOOLCHAIN=\"{}\" COMPONENTS=\"{RUST_COMPONENTS}\" /tmp/djinn-scripts/install-rust.sh",
        space_join(toolchains.iter().copied()),
        rust.default_toolchain
    );
    writeln!(df, "{line}").unwrap();
}

/// Workspace overrides come from either `workspace.toolchain` (rust)
/// or `workspace.version` (node/python/go). For Rust specifically we
/// use `toolchain` and de-dup against the language default.
fn aggregate_rust_toolchains<'a>(
    rust: &'a RustLanguage,
    config: &'a EnvironmentConfig,
) -> Vec<&'a str> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut out: Vec<&str> = Vec::new();
    for tc in std::iter::once(rust.default_toolchain.as_str()).chain(
        config
            .workspaces
            .iter()
            .filter(|ws| ws.language == "rust")
            .filter_map(|ws| ws.toolchain.as_deref()),
    ) {
        if seen.insert(tc) {
            out.push(tc);
        }
    }
    out
}

fn emit_node_block(df: &mut String, languages: &Languages, config: &EnvironmentConfig) {
    let Some(node) = &languages.node else { return };
    let versions = aggregate_node_versions(node, config);
    let mut line = format!(
        "RUN NODE_VERSIONS=\"{}\" DEFAULT_NODE=\"{}\"",
        space_join(versions.iter().copied()),
        node.default_version
    );
    let package_managers = aggregate_node_pms(node, config);
    if !package_managers.is_empty() {
        line.push_str(&format!(
            " PACKAGE_MANAGERS=\"{}\"",
            space_join(package_managers.iter().copied())
        ));
    }
    // scip-typescript is the right indexer for both TS and JS workspaces —
    // there's no separate `typescript` block in `Languages`, so we key on
    // `node` being present.
    line.push_str(&format!(
        " SCIP_INDEXER=\"{TYPESCRIPT_SCIP_INDEXER}\" SCIP_TYPESCRIPT_VERSION=\"{SCIP_TYPESCRIPT_VERSION}\""
    ));
    line.push_str(" /tmp/djinn-scripts/install-node.sh");
    writeln!(df, "{line}").unwrap();
}

fn aggregate_node_versions<'a>(
    node: &'a NodeLanguage,
    config: &'a EnvironmentConfig,
) -> Vec<&'a str> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut out: Vec<&str> = Vec::new();
    for v in std::iter::once(node.default_version.as_str()).chain(
        config
            .workspaces
            .iter()
            .filter(|ws| ws.language == "node")
            .filter_map(|ws| ws.version.as_deref()),
    ) {
        if seen.insert(v) {
            out.push(v);
        }
    }
    out
}

fn aggregate_node_pms<'a>(node: &'a NodeLanguage, config: &'a EnvironmentConfig) -> Vec<&'a str> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut out: Vec<&str> = Vec::new();
    for pm in node.default_package_manager.as_deref().into_iter().chain(
        config
            .workspaces
            .iter()
            .filter(|ws| ws.language == "node")
            .filter_map(|ws| ws.package_manager.as_deref()),
    ) {
        if seen.insert(pm) {
            out.push(pm);
        }
    }
    out
}

// SCIP indexer is hard-coded per language. Previously the env-config
// carried a per-language override, but in practice the mapping is fixed
// (scip-python for python, scip-go for go, etc.) — the 2026-04-22 cleanup
// dropped the field.
const PYTHON_SCIP_INDEXER: &str = "scip-python";
const GO_SCIP_INDEXER: &str = "scip-go";
const TYPESCRIPT_SCIP_INDEXER: &str = "scip-typescript";
const JAVA_SCIP_INDEXER: &str = "scip-java";
const CLANG_SCIP_INDEXER: &str = "scip-clang";
const RUBY_SCIP_INDEXER: &str = "scip-ruby";
const DOTNET_SCIP_INDEXER: &str = "scip-dotnet";

// Version pins for SCIP indexers. `"latest"` means the Go-module-proxy /
// PyPI / npm `latest` tag at build time. Bump these consts to roll
// forward; pin to a known-good tag when an upstream `@latest` regresses.
// See `project_scip_indexer_versions.md` in user memory for the running
// list of known regressions.
//
// Known regressions:
//   - scip-go v0.2.4 panics on some Go monorepos:
//     `runtime error: index out of range [0] with length 0` in
//     `internal/index/scip.go:221` (`indexVisitPackages`). Fixed in
//     v0.2.6 via the panic-to-error conversions in upstream PR #196
//     and the test-only-dir fix in #255 — hence the pin below.
//
// Format notes (per each install-*.sh):
//   - scip-go: Go module-proxy selector, leading `v` (e.g. `v0.2.6`).
//   - scip-java / scip-clang: GitHub release tag, leading `v`.
//   - scip-ruby: RubyGems version, BARE number (no leading `v`); the
//     upstream tag is `scip-ruby-v0.4.7` but the gem ships as `0.4.7`.
//   - scip-dotnet: NuGet version for `dotnet tool install --version`,
//     BARE number (no leading `v`).
//   - scip-python: npm version (@sourcegraph/scip-python), BARE number
//     (no leading `v`). NOT on PyPI — it is published only to npm, so the
//     python image installs it via npm (see install-python.sh).
//   - scip-typescript: npm version, BARE number (no leading `v`).
const SCIP_GO_VERSION: &str = "v0.2.6";
// Pinned to a known-good npm release rather than `latest`: `@latest` was the
// indirect cause of the prior breakage (the old PyPI path 404'd on every
// build). Bump deliberately when rolling the indexer forward.
const SCIP_PYTHON_VERSION: &str = "0.6.6";
const SCIP_TYPESCRIPT_VERSION: &str = "latest";
const SCIP_JAVA_VERSION: &str = "v0.12.3";
const SCIP_CLANG_VERSION: &str = "v0.4.0";
const SCIP_RUBY_VERSION: &str = "0.4.7";
const SCIP_DOTNET_VERSION: &str = "0.2.14";

fn emit_python_block(df: &mut String, languages: &Languages, config: &EnvironmentConfig) {
    let Some(python) = &languages.python else {
        return;
    };
    let versions = aggregate_simple(&python.default_version, config, "python");
    let line = format!(
        "RUN PYTHON_VERSIONS=\"{}\" DEFAULT_PYTHON=\"{}\" SCIP_INDEXER=\"{PYTHON_SCIP_INDEXER}\" SCIP_PYTHON_VERSION=\"{SCIP_PYTHON_VERSION}\" /tmp/djinn-scripts/install-python.sh",
        space_join(versions.iter().copied()),
        python.default_version
    );
    writeln!(df, "{line}").unwrap();
}

fn emit_go_block(df: &mut String, languages: &Languages, _config: &EnvironmentConfig) {
    let Some(go) = &languages.go else { return };
    // Go is intentionally single-version — multi-toolchain is handled
    // by `go install golang.org/dl/go<X>` at runtime, not at image build.
    let line = format!(
        "RUN GO_VERSION=\"{}\" SCIP_INDEXER=\"{GO_SCIP_INDEXER}\" SCIP_GO_VERSION=\"{SCIP_GO_VERSION}\" /tmp/djinn-scripts/install-go.sh",
        go.default_version
    );
    writeln!(df, "{line}").unwrap();
}

fn emit_java_block(df: &mut String, languages: &Languages, _config: &EnvironmentConfig) {
    let Some(java) = &languages.java else { return };
    writeln!(
        df,
        "RUN JAVA_VERSION=\"{}\" SCIP_INDEXER=\"{JAVA_SCIP_INDEXER}\" SCIP_JAVA_VERSION=\"{SCIP_JAVA_VERSION}\" /tmp/djinn-scripts/install-java.sh",
        java.default_version
    )
    .unwrap();
}

fn emit_ruby_block(df: &mut String, languages: &Languages, _config: &EnvironmentConfig) {
    let Some(ruby) = &languages.ruby else { return };
    writeln!(
        df,
        "RUN RUBY_VERSION=\"{}\" SCIP_INDEXER=\"{RUBY_SCIP_INDEXER}\" SCIP_RUBY_VERSION=\"{SCIP_RUBY_VERSION}\" /tmp/djinn-scripts/install-ruby.sh",
        ruby.default_version
    )
    .unwrap();
}

fn emit_dotnet_block(df: &mut String, languages: &Languages) {
    let Some(d) = &languages.dotnet else { return };
    writeln!(
        df,
        "RUN DOTNET_VERSION=\"{}\" SCIP_INDEXER=\"{DOTNET_SCIP_INDEXER}\" SCIP_DOTNET_VERSION=\"{SCIP_DOTNET_VERSION}\" /tmp/djinn-scripts/install-dotnet.sh",
        d.default_version
    )
    .unwrap();
}

fn emit_clang_block(df: &mut String, languages: &Languages) {
    let Some(c) = &languages.clang else { return };
    writeln!(
        df,
        "RUN CLANG_VERSION=\"{}\" SCIP_INDEXER=\"{CLANG_SCIP_INDEXER}\" SCIP_CLANG_VERSION=\"{SCIP_CLANG_VERSION}\" /tmp/djinn-scripts/install-clang.sh",
        c.default_version
    )
    .unwrap();
}

/// Build a unique, ordered list of `default_version` + all workspace
/// `version` overrides for a given language slug. Used by language
/// blocks whose workspace overrides map onto `version` rather than
/// `toolchain`.
fn aggregate_simple<'a>(
    default_version: &'a str,
    config: &'a EnvironmentConfig,
    language: &str,
) -> Vec<&'a str> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut out: Vec<&str> = Vec::new();
    for v in std::iter::once(default_version).chain(
        config
            .workspaces
            .iter()
            .filter(|ws| ws.language == language)
            .filter_map(|ws| ws.version.as_deref()),
    ) {
        if seen.insert(v) {
            out.push(v);
        }
    }
    out
}

fn emit_env(df: &mut String, config: &EnvironmentConfig) {
    for (k, v) in &config.env {
        // validate() already rejected newlines / NULs / shell-unsafe keys;
        // we still quote the value to be safe against spaces.
        writeln!(df, "ENV {k}={}", shell_quote(v)).unwrap();
    }
}

fn emit_post_build_hooks(df: &mut String, config: &EnvironmentConfig) {
    for hook in &config.lifecycle.post_build {
        emit_hook_as_run(df, hook);
    }
}

fn emit_hook_as_run(df: &mut String, hook: &HookCommand) {
    match hook {
        HookCommand::Shell(s) => {
            // Put every hook on its own `RUN`; keeps layer count
            // proportional to hook count so a single tweak invalidates
            // only the layers below it.
            writeln!(df, "RUN {s}").unwrap();
        }
        HookCommand::Exec(argv) => {
            // Build-time `RUN` in exec form is JSON-array syntax.
            writeln!(df, "RUN {}", render_json_array(argv)).unwrap();
        }
        HookCommand::Parallel(map) => {
            // Parallel at build time would need `&` + `wait`; keep it
            // simple and run sequentially. Named for log clarity.
            for (name, inner) in map {
                writeln!(df, "# hook: {name}").unwrap();
                emit_hook_as_run(df, inner);
            }
        }
    }
}

fn emit_cleanup(df: &mut String) {
    writeln!(df, "RUN rm -rf /tmp/djinn-scripts").unwrap();
    // Make RUSTUP_HOME + CARGO_HOME writable for the non-root djinn user
    // so workspace-pinned toolchains (rust-toolchain.toml → e.g. 1.94.1)
    // can be installed at session time without rustup falling back to
    // .rustup/ inside the workspace. We keep the install paths at
    // /usr/local/{rustup,cargo} because they live in image layers (not a
    // PVC mount), so the baked-in stable toolchain stays visible at
    // runtime. Best-effort — directories may not exist for
    // language-less image variants; `|| true` keeps the build clean.
    writeln!(
        df,
        "RUN [ -d /usr/local/rustup ] && chmod -R a+rwX /usr/local/rustup || true; \
         [ -d /usr/local/cargo ] && chmod -R a+rwX /usr/local/cargo || true"
    )
    .unwrap();
}

// ---- small helpers ------------------------------------------------------

fn space_join<'a, I: IntoIterator<Item = &'a str>>(iter: I) -> String {
    let mut out = String::new();
    for (i, s) in iter.into_iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(s);
    }
    out
}

fn space_join_sorted(items: &[String]) -> String {
    let mut v: Vec<&str> = items.iter().map(String::as_str).collect();
    v.sort();
    v.dedup();
    space_join(v.iter().copied())
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':' | ','))
        && !value.is_empty()
    {
        value.to_string()
    } else {
        let escaped = value.replace('"', "\\\"");
        format!("\"{escaped}\"")
    }
}

fn render_json_array(argv: &[String]) -> String {
    let parts: Vec<String> = argv
        .iter()
        .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("[{}]", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_worker() -> AgentWorkerImage {
        AgentWorkerImage::new("djinn/agent-worker", "sha256-deadbeef")
    }

    fn minimal_valid_config() -> EnvironmentConfig {
        let mut cfg = EnvironmentConfig::empty();
        cfg.schema_version = djinn_stack::environment::SCHEMA_VERSION;
        cfg
    }

    #[test]
    fn empty_config_emits_base_and_worker_only() {
        let df = generate_dockerfile(
            &minimal_valid_config(),
            &agent_worker(),
            DEFAULT_LAUNCHER_PROTOCOL,
        )
        .unwrap();
        assert!(df.dockerfile.contains("FROM debian:trixie-slim"));
        assert!(
            df.dockerfile
                .contains("COPY --from=djinn/agent-worker:sha256-deadbeef")
        );
        // The cgroup-launcher binary ships in the SAME per-project image (copied
        // from the agent-worker stage) so the rendered launcher sidecar command
        // resolves to a real packaged artifact — no separate/fabricated image.
        assert!(
            df.dockerfile.contains(
                "/usr/local/bin/djinn-cgroup-launcher /opt/djinn/bin/djinn-cgroup-launcher"
            ),
            "per-project image must package the cgroup-launcher binary at /opt/djinn/bin"
        );
        assert!(df.dockerfile.contains("install-agent-worker.sh"));
        assert!(!df.dockerfile.contains("install-rust.sh"));
    }

    /// The artifact declares its launcher authority protocol in its own build
    /// metadata, using the canonical wire form, and reports the same value on
    /// the [`BuildContext`] so the build Job never has to guess.
    #[test]
    fn the_artifact_declares_its_launcher_protocol_in_build_metadata() {
        let df = generate_dockerfile(
            &minimal_valid_config(),
            &agent_worker(),
            DEFAULT_LAUNCHER_PROTOCOL,
        )
        .unwrap();

        assert_eq!(df.launcher_protocol, DEFAULT_LAUNCHER_PROTOCOL);
        assert!(
            df.dockerfile.contains(&format!(
                "LABEL {LAUNCHER_PROTOCOL_LABEL}=\"{}\"",
                DEFAULT_LAUNCHER_PROTOCOL.as_wire()
            )),
            "the OCI label is the declaration; without it the artifact says nothing:\n{}",
            df.dockerfile
        );
        assert!(df.dockerfile.contains(&format!(
            "ENV {LAUNCHER_PROTOCOL_ENV}={}",
            DEFAULT_LAUNCHER_PROTOCOL.as_wire()
        )));

        // The declaration describes the launcher binary, so it must land after
        // the COPY that puts it there.
        let copy = df
            .dockerfile
            .find("/opt/djinn/bin/djinn-cgroup-launcher")
            .expect("launcher COPY");
        let label = df.dockerfile.find(LAUNCHER_PROTOCOL_LABEL).expect("label");
        assert!(copy < label);
    }

    /// **The declaration is configurable, and the configured value is what the
    /// artifact carries.** Every protocol the type admits — not just the
    /// default — reaches the OCI label, the sidecar env var, and the
    /// [`BuildContext`] the build Job reports from.
    ///
    /// MUTATION: pass `DEFAULT_LAUNCHER_PROTOCOL` to `emit_launcher_protocol`
    /// instead of the argument (i.e. restore the hardcoded declaration). The
    /// `resize-v2` iteration fails on the LABEL assertion.
    #[test]
    fn the_configured_protocol_is_what_the_artifact_declares() {
        for protocol in LauncherAuthorityProtocol::ALL {
            let df = generate_dockerfile(&minimal_valid_config(), &agent_worker(), protocol)
                .unwrap_or_else(|e| panic!("{protocol} must generate: {e}"));
            let wire = protocol.as_wire();

            assert_eq!(df.launcher_protocol, protocol);
            assert!(
                df.dockerfile
                    .contains(&format!("LABEL {LAUNCHER_PROTOCOL_LABEL}=\"{wire}\"")),
                "{protocol}: the built artifact must carry the CONFIGURED declaration:\n{}",
                df.dockerfile
            );
            assert!(
                df.dockerfile
                    .contains(&format!("ENV {LAUNCHER_PROTOCOL_ENV}={wire}")),
                "{protocol}: the sidecar reads its protocol out of the same image:\n{}",
                df.dockerfile
            );
            // Nothing else in the artifact may claim a different protocol.
            for other in LauncherAuthorityProtocol::ALL {
                if other != protocol {
                    assert!(
                        !df.dockerfile.contains(other.as_wire()),
                        "{protocol}: the artifact also mentions {other}"
                    );
                }
            }
            df.verify_declaration().unwrap_or_else(|e| {
                panic!("{protocol}: a freshly generated context must agree: {e}")
            });
        }
    }

    /// **A declaration the build context does not back cannot reach a build.**
    /// `verify_declaration` is what `build_image_build_job` calls before it
    /// renders the sentinel that becomes the catalog row, so a context whose
    /// reported protocol drifts from the Dockerfile it carries is rejected
    /// rather than built.
    ///
    /// MUTATION: make `verify_declaration` return `Ok(self.launcher_protocol)`
    /// unconditionally. Every arm below fails.
    #[test]
    fn a_context_that_reports_something_other_than_it_built_is_refused() {
        let good = generate_dockerfile(
            &minimal_valid_config(),
            &agent_worker(),
            LauncherAuthorityProtocol::LeafV1,
        )
        .unwrap();
        assert_eq!(
            good.verify_declaration().unwrap(),
            LauncherAuthorityProtocol::LeafV1
        );

        // The catalog would be told `resize-v2` about a `leaf-v1` artifact.
        let mut lying = good.clone();
        lying.launcher_protocol = LauncherAuthorityProtocol::ResizeV2;
        assert_eq!(
            lying.verify_declaration(),
            Err(DeclarationError::Disagree {
                dockerfile_says: LauncherAuthorityProtocol::LeafV1,
                context_says: LauncherAuthorityProtocol::ResizeV2,
            })
        );

        // A Dockerfile that lost its declaration entirely.
        let mut stripped = good.clone();
        stripped.dockerfile = stripped
            .dockerfile
            .lines()
            .filter(|line| !line.contains(LAUNCHER_PROTOCOL_LABEL))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            stripped.verify_declaration(),
            Err(DeclarationError::Undeclared {
                context_says: LauncherAuthorityProtocol::LeafV1,
            })
        );

        // The label and the env var disagreeing is fatal too: the catalog reads
        // one, the sidecar reads the other.
        let mut split = good.clone();
        split.dockerfile = split.dockerfile.replace(
            &format!("ENV {LAUNCHER_PROTOCOL_ENV}=leaf-v1"),
            &format!("ENV {LAUNCHER_PROTOCOL_ENV}=resize-v2"),
        );
        assert!(matches!(
            split.verify_declaration(),
            Err(DeclarationError::LabelEnvDisagree { .. })
        ));

        // A second declaration appended later is not "last one wins".
        let mut doubled = good.clone();
        doubled
            .dockerfile
            .push_str(&format!("LABEL {LAUNCHER_PROTOCOL_LABEL}=\"resize-v2\"\n"));
        assert!(matches!(
            doubled.verify_declaration(),
            Err(DeclarationError::Undeclared { .. })
        ));
    }

    /// The declaration is not derived from — and cannot be confused with — the
    /// image's name. `generate_dockerfile` is never given a tag, and the wire
    /// string comes from the canonical type rather than a local literal.
    #[test]
    fn the_declaration_is_build_metadata_not_a_name() {
        let df = generate_dockerfile(
            &minimal_valid_config(),
            &agent_worker(),
            DEFAULT_LAUNCHER_PROTOCOL,
        )
        .unwrap();
        assert_eq!(DEFAULT_LAUNCHER_PROTOCOL.as_wire(), "leaf-v1");
        assert_eq!(
            df.dockerfile
                .matches(DEFAULT_LAUNCHER_PROTOCOL.as_wire())
                .count(),
            2,
            "the protocol appears exactly twice — the LABEL and the ENV — and nowhere else"
        );
    }

    #[test]
    fn missing_worker_ref_rejected() {
        let err = generate_dockerfile(
            &minimal_valid_config(),
            &AgentWorkerImage::new("djinn/agent-worker", ""),
            DEFAULT_LAUNCHER_PROTOCOL,
        )
        .unwrap_err();
        assert!(matches!(err, DockerfileError::MissingAgentWorkerRef));
    }

    #[test]
    fn two_rust_toolchains_aggregate_into_single_run_line() {
        // The motivating case: two Rust workspaces pinned to different
        // toolchains in one project.
        let mut cfg = minimal_valid_config();
        cfg.languages.rust = Some(djinn_stack::environment::RustLanguage {
            default_toolchain: "stable".into(),
        });
        cfg.workspaces = vec![
            djinn_stack::environment::Workspace {
                slug: None,
                name: None,
                tags: Vec::new(),
                root: "server".into(),
                language: "rust".into(),
                toolchain: Some("stable".into()),
                version: None,
                package_manager: None,
                cargo_features: Vec::new(),
                cargo_all_features: false,
            },
            djinn_stack::environment::Workspace {
                slug: None,
                name: None,
                tags: Vec::new(),
                root: "tools/codegen".into(),
                language: "rust".into(),
                toolchain: Some("1.85.0".into()),
                version: None,
                package_manager: None,
                cargo_features: Vec::new(),
                cargo_all_features: false,
            },
        ];
        let df = generate_dockerfile(&cfg, &agent_worker(), DEFAULT_LAUNCHER_PROTOCOL).unwrap();
        // A single RUN line with both toolchains, default preserved,
        // components carried through.
        assert!(
            df.dockerfile.contains("TOOLCHAINS=\"stable 1.85.0\""),
            "dockerfile:\n{}",
            df.dockerfile
        );
        assert!(df.dockerfile.contains("DEFAULT_TOOLCHAIN=\"stable\""));
        assert!(
            df.dockerfile
                .contains("COMPONENTS=\"rust-analyzer clippy rustfmt\"")
        );
    }

    #[test]
    fn node_aggregates_workspace_versions_and_pms() {
        let mut cfg = minimal_valid_config();
        cfg.languages.node = Some(djinn_stack::environment::NodeLanguage {
            default_version: "22".into(),
            default_package_manager: Some("pnpm".into()),
        });
        cfg.workspaces = vec![djinn_stack::environment::Workspace {
            slug: None,
            name: None,
            tags: Vec::new(),
            root: "legacy/ui".into(),
            language: "node".into(),
            toolchain: None,
            version: Some("20".into()),
            package_manager: Some("yarn".into()),
            cargo_features: Vec::new(),
            cargo_all_features: false,
        }];
        let df = generate_dockerfile(&cfg, &agent_worker(), DEFAULT_LAUNCHER_PROTOCOL).unwrap();
        assert!(df.dockerfile.contains("NODE_VERSIONS=\"22 20\""));
        assert!(df.dockerfile.contains("PACKAGE_MANAGERS=\"pnpm yarn\""));
    }

    #[test]
    fn system_packages_sort_deduplicate_and_become_env_inline() {
        let mut cfg = minimal_valid_config();
        cfg.system_packages = vec![
            "postgresql-client".into(),
            "jq".into(),
            "jq".into(),
            "build-essential".into(),
        ];
        let df = generate_dockerfile(&cfg, &agent_worker(), DEFAULT_LAUNCHER_PROTOCOL).unwrap();
        assert!(df.dockerfile.contains(
            "APT_PACKAGES=\"build-essential jq postgresql-client\" /tmp/djinn-scripts/install-system.sh"
        ));
    }

    #[test]
    fn env_key_emits_env_line() {
        let mut cfg = minimal_valid_config();
        cfg.env.insert("RUST_LOG".into(), "info".into());
        cfg.env.insert("CI".into(), "true".into());
        let df = generate_dockerfile(&cfg, &agent_worker(), DEFAULT_LAUNCHER_PROTOCOL).unwrap();
        // BTreeMap ordering is alphabetical → CI first, then RUST_LOG.
        let ci_pos = df.dockerfile.find("ENV CI=true").unwrap();
        let rust_pos = df.dockerfile.find("ENV RUST_LOG=info").unwrap();
        assert!(ci_pos < rust_pos);
    }

    #[test]
    fn post_build_hooks_emit_run_lines() {
        let mut cfg = minimal_valid_config();
        cfg.lifecycle.post_build = vec![
            HookCommand::Shell("echo build".into()),
            HookCommand::Exec(vec!["bash".into(), "-lc".into(), "echo exec".into()]),
        ];
        let df = generate_dockerfile(&cfg, &agent_worker(), DEFAULT_LAUNCHER_PROTOCOL).unwrap();
        assert!(df.dockerfile.contains("RUN echo build"));
        assert!(
            df.dockerfile
                .contains(r#"RUN ["bash", "-lc", "echo exec"]"#),
            "actual:\n{}",
            df.dockerfile
        );
    }

    #[test]
    fn unknown_workspace_language_rejected() {
        let mut cfg = minimal_valid_config();
        cfg.workspaces = vec![djinn_stack::environment::Workspace {
            slug: None,
            name: None,
            tags: Vec::new(),
            root: "src".into(),
            language: "zig".into(),
            toolchain: None,
            version: None,
            package_manager: None,
            cargo_features: Vec::new(),
            cargo_all_features: false,
        }];
        let err =
            generate_dockerfile(&cfg, &agent_worker(), DEFAULT_LAUNCHER_PROTOCOL).unwrap_err();
        assert!(matches!(err, DockerfileError::UnknownWorkspaceLanguage(s) if s == "zig"));
    }

    #[test]
    fn base_image_is_hard_coded_to_debian_bookworm_slim() {
        // Alpine support was dropped in the 2026-04-22 cleanup; the base
        // is now fixed regardless of what the config carried.
        let cfg = minimal_valid_config();
        let df = generate_dockerfile(&cfg, &agent_worker(), DEFAULT_LAUNCHER_PROTOCOL).unwrap();
        assert!(df.dockerfile.contains("FROM debian:trixie-slim"));
        assert!(df.dockerfile.contains("/tmp/djinn-scripts/base-debian.sh"));
    }
}
