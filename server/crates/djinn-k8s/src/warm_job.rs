//! Pure `Job` manifest builder for a per-project canonical-graph warm run.
//!
//! The warm Job runs `djinn-agent-worker warm-graph <project_id>` inside
//! the project's devcontainer image. The devcontainer carries language
//! indexers (rust-analyzer for SCIP, etc.) so `warm-graph` can drive the
//! SCIP pipeline natively there. The `djinn-agent-worker` binary gains
//! a `warm-graph` subcommand that delegates into `djinn-graph` (the
//! extracted canonical-graph crate both server and worker depend on).
//!
//! The Pod's command is a shell wrapper that first `git clone`s the bare
//! mirror into an emptyDir workspace, then execs `djinn-agent-worker
//! warm-graph`. `DJINN_PROJECT_ROOT` tells the binary to treat the clone
//! as the project's working tree (bypassing the DB's stored
//! `projects.path` which points at a server-local dir not available in
//! the warm Pod). `backoffLimit: 0` — if the warm fails we rely on the
//! next graph_warmer tick to trigger a fresh attempt.

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, PersistentVolumeClaimVolumeSource, PodSpec,
    PodTemplateSpec, ResourceRequirements, Toleration, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use uuid::Uuid;

use crate::config::KubernetesConfig;

/// Label key identifying a graph-warm Job.
pub const LABEL_WARM: &str = "djinn.app/warm";
/// Label key for the project id a warm Job targets.
pub const LABEL_PROJECT_ID: &str = "djinn.app/project-id";
/// `djinn.app/component` value written on warm resources.
pub const COMPONENT_GRAPH_WARM: &str = "graph-warm";
/// Label key identifying which djinn-internal component created the resource.
pub const LABEL_COMPONENT: &str = "djinn.app/component";

/// Mount path for the read-only mirror PVC (mirrors the task-run Job).
pub const MIRROR_MOUNT_DIR: &str = "/mirror";
/// Volume name for the read-only mirror PVC.
pub const VOLUME_MIRROR: &str = "mirror";
/// Mount path for the writable workspace emptyDir. The warm Pod clones
/// the bare mirror here before running `warm-graph`.
pub const WORKSPACE_MOUNT_DIR: &str = "/workspace";
/// Volume name for the workspace emptyDir.
pub const VOLUME_WORKSPACE: &str = "workspace";

/// Binary path inside the devcontainer image. The `djinn-agent-worker`
/// Feature installs the worker binary at `/opt/djinn/bin/djinn-agent-worker`.
/// Both the warm Pod (`warm-graph <project-id>`) and the task-run Pod
/// (`task-run`, see [`crate::job::build_task_run_job`]) invoke this path
/// explicitly rather than relying on the image ENTRYPOINT — the
/// devcontainer base image's ENTRYPOINT typically launches a shell, not
/// the worker binary.
pub const WARM_COMMAND_BIN: &str = "/opt/djinn/bin/djinn-agent-worker";

/// Build the Job manifest dispatched for one graph-warm run.
///
/// `project_id` becomes the resource-name suffix + label value. The
/// Pod's command is a shell wrapper that clones the bare mirror into a
/// writable emptyDir, then invokes `djinn-server warm-graph <project_id>`.
///
/// The ServiceAccount (`config.service_account`) is reused from task-run
/// dispatch — the warm Pod needs the mirror PVC + the DB env, both of
/// which already work with the task-run SA.
pub fn build_warm_job(
    config: &KubernetesConfig,
    project_id: &str,
    project_image_tag: &str,
    policy: Option<&djinn_stack::environment::CargoCachePolicy>,
) -> Job {
    let suffix = Uuid::now_v7();
    let sanitized_project = sanitize_id(project_id);
    let job_name = format!("djinn-warm-{}-{}", sanitized_project, short_uuid(&suffix));
    let labels = job_labels(project_id);

    let project_root = format!("{WORKSPACE_MOUNT_DIR}/{sanitized_project}");
    let mirror_path = format!("{MIRROR_MOUNT_DIR}/{project_id}.git");

    // Shell wrapper: the bare mirror on the PVC is `--filter=blob:none`,
    // so cloning it with `--local --shared` gives a partial clone where
    // `git checkout` fails on every missing blob (`unable to read sha1
    // file of <path>`). We avoid the filter entirely by pulling the
    // upstream URL (with fresh installation token, rotated every 60s by
    // the mirror fetcher) out of the mirror config and doing a clone
    // straight from GitHub. Same pattern the per-project build Job uses.
    //
    // `--depth 1000 --single-branch`: SCIP only needs HEAD source, but
    // the coupling-index phase walks `cursor..HEAD` and needs the saved
    // cursor's history to be reachable. `--depth 1000` gives ~1000
    // commits of recent history for free (the typical warm cadence is
    // <100 new commits, so the saved cursor almost always lands inside
    // this window) and only adds a few MB over `--depth 1` for typical
    // repos thanks to git's pack deduplication. Cursors older than 1000
    // commits are handled by `coupling_index::try_fetch_cursor` falling
    // back to `git fetch --unshallow`. See
    // `cases/plan-a-warm-cargo-base-reuse-validated-working-v0-6-11-0-6-12`
    // for the broader warm-cost discussion.
    let cmd = format!(
        r#"set -euo pipefail
git config --global --add safe.directory "{mirror_path}"
UPSTREAM_URL="$(git -C "{mirror_path}" config remote.origin.url)"
git clone --depth 1000 --single-branch "$UPSTREAM_URL" "{project_root}"
# Install JS deps before indexing so scip-typescript can resolve
# tsconfig `extends` that point at workspace packages (e.g. a shared
# `tsconfig` package in a pnpm/turbo monorepo) — those live under
# node_modules, so without an install every `tsconfig.json` fails to load
# and the TS indexer reports "missing tsconfig.json". Gated on a lockfile
# so non-JS repos are untouched; `|| true` keeps a JS-install failure from
# aborting a warm whose Rust/Python/Go indexers would still succeed.
cd "{project_root}"
if [ -f pnpm-lock.yaml ]; then
  ( corepack enable >/dev/null 2>&1 || true; \
    corepack pnpm install --frozen-lockfile || pnpm install --frozen-lockfile || pnpm install ) || true
elif [ -f yarn.lock ]; then
  ( corepack enable >/dev/null 2>&1 || true; \
    corepack yarn install --frozen-lockfile || yarn install ) || true
elif [ -f package-lock.json ]; then
  ( npm ci || npm install ) || true
fi
# The cargo target base is warmed by `warm-graph` itself (in the worker), NOT
# here: the worker normalizes tracked-file mtimes to commit times — the SAME
# normalization task-run applies before it compiles — then compiles the
# cargo workspace into the warm base. Doing it in this shell wrapper (with
# clone-time mtimes, and gated on a root `Cargo.toml` that djinn's `server/`
# workspace doesn't have) produced a base whose cargo fingerprints never matched
# task-run's tree, so task-run recompiled cold every run. See
# `warm_cargo_target_base` in djinn-agent-worker.
exec {bin} warm-graph "{project_id}"
"#,
        mirror_path = mirror_path,
        project_root = project_root,
        bin = WARM_COMMAND_BIN,
        project_id = project_id,
    );

    let mut env = vec![
        env_var("DJINN_MIRROR_ROOT", MIRROR_MOUNT_DIR),
        env_var("DJINN_WARM_PROJECT_ID", project_id),
        // run_warm_graph_command picks this up when set and uses it as
        // the canonical project root, bypassing the DB's server-local
        // `projects.path`.
        env_var("DJINN_PROJECT_ROOT", &project_root),
        // Verbose logging for djinn crates so SCIP indexer discovery +
        // invocation failures surface in the Pod log instead of being
        // silently absent.
        env_var("RUST_LOG", "info,djinn=debug"),
    ];
    // Forward the server's DB connection so `bootstrap_warm_database` in
    // djinn-agent-worker reaches the same Postgres instance as the server.
    // The worker hard-requires `DJINN_DATABASE_URL` (it errors out with
    // "DJINN_DATABASE_URL must be set for the warm worker pod" if absent)
    // — the postgres cut-over renamed this from DJINN_MYSQL_URL, and the
    // task-run path (job.rs) was updated but this warm path was missed.
    // DJINN_SERVER_ADDR is intentionally absent — `warm-graph` doesn't
    // dial djinn-server.
    if let Some(url) = config.database_url.as_deref() {
        env.push(env_var("DJINN_DATABASE_URL", url));
    }
    // Route the Rust toolchain caches (CARGO_HOME/CARGO_TARGET_DIR/SCCACHE_DIR)
    // to the /cache PVC. Warm Pods intentionally write the shared per-project
    // target base with incremental compilation disabled; task-run Pods use
    // private run target dirs but keep the same shared CARGO_HOME/SCCACHE
    // settings. Single-sourced in job.rs to avoid the
    // task-run-updated-but-warm-missed drift that bit DJINN_*_URL.
    // Needs the cache volume mounted below.
    env.extend(crate::job::warm_cache_env_vars(project_id, policy));

    let container = Container {
        name: "warmer".to_string(),
        image: Some(project_image_tag.to_string()),
        image_pull_policy: Some(config.image_pull_policy.clone()),
        command: Some(vec!["/bin/bash".to_string(), "-c".to_string(), cmd]),
        env: Some(env),
        volume_mounts: Some(vec![
            VolumeMount {
                name: VOLUME_MIRROR.to_string(),
                mount_path: MIRROR_MOUNT_DIR.to_string(),
                read_only: Some(true),
                ..VolumeMount::default()
            },
            VolumeMount {
                name: VOLUME_WORKSPACE.to_string(),
                mount_path: WORKSPACE_MOUNT_DIR.to_string(),
                read_only: Some(false),
                ..VolumeMount::default()
            },
            // Shared cache PVC backing the warm-owned cargo target base plus the
            // shared registry/sccache dirs. Task-run Pods mount the same PVC but
            // use private cargo target dirs.
            VolumeMount {
                name: crate::job::VOLUME_CACHE.to_string(),
                mount_path: crate::job::CACHE_MOUNT_DIR.to_string(),
                read_only: Some(false),
                ..VolumeMount::default()
            },
            crate::env_config::env_config_volume_mount(),
        ]),
        // Warm Pod was previously unbounded — SCIP indexer subprocesses
        // can spike CPU/memory fast on a medium Rust workspace. Set
        // explicit requests + limits so the kubelet has scheduling +
        // OOM signals (see Gap 4 of the Phase 7 audit).
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(config.warm_cpu_request.clone())),
                (
                    "memory".to_string(),
                    Quantity(config.warm_memory_request.clone()),
                ),
            ])),
            limits: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(config.warm_cpu_limit.clone())),
                (
                    "memory".to_string(),
                    Quantity(config.warm_memory_limit.clone()),
                ),
            ])),
            ..ResourceRequirements::default()
        }),
        ..Container::default()
    };

    let volumes = vec![
        Volume {
            name: VOLUME_MIRROR.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: config.mirror_pvc.clone(),
                read_only: Some(true),
            }),
            ..Volume::default()
        },
        Volume {
            name: VOLUME_WORKSPACE.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Volume::default()
        },
        Volume {
            name: crate::job::VOLUME_CACHE.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: config.cache_pvc.clone(),
                read_only: Some(false),
            }),
            ..Volume::default()
        },
        crate::env_config::env_config_volume(project_id),
    ];

    // Warm Pods must land on the same NodePool as the task-runs they pre-warm
    // — otherwise the warmup is wasted. Both fields are `None` when no
    // operator scheduling hints are configured, keeping the manifest shape
    // unchanged for existing installs.
    let node_selector = (!config.node_selector.is_empty()).then(|| config.node_selector.clone());
    let tolerations: Option<Vec<Toleration>> =
        (!config.tolerations.is_empty()).then(|| config.tolerations.clone());

    let pod_spec = PodSpec {
        service_account_name: Some(config.service_account.clone()),
        restart_policy: Some("Never".to_string()),
        containers: vec![container],
        volumes: Some(volumes),
        node_selector,
        tolerations,
        // Run as uid 10001 like task-runs (job.rs). The warm pod shares the
        // /cache cargo target PVC with workers; without this it runs as root
        // and writes root-owned cargo artifacts the worker (uid 10001) can't
        // overwrite, corrupting the shared cache.
        security_context: Some(k8s_openapi::api::core::v1::PodSecurityContext {
            run_as_user: Some(10001),
            run_as_group: Some(10001),
            ..Default::default()
        }),
        ..PodSpec::default()
    };

    let template = PodTemplateSpec {
        metadata: Some(ObjectMeta {
            labels: Some(labels.clone()),
            ..ObjectMeta::default()
        }),
        spec: Some(pod_spec),
    };

    Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: Some(config.namespace.clone()),
            labels: Some(labels),
            ..ObjectMeta::default()
        },
        spec: Some(JobSpec {
            template,
            backoff_limit: Some(0),
            ttl_seconds_after_finished: Some(config.warm_job_ttl_seconds),
            // Deadline margin: `warm_cargo_target_base` compiles a single
            // default-features pass (clippy + build + test-compile) matching
            // the worker's feature set. A cold first warm takes ~20-25 min
            // for a ~12-crate workspace. The default `warm_job_timeout_seconds`
            // is 3600s (60 min), leaving ~35 min of margin. If a larger
            // workspace consistently hits this deadline the warm Pod is
            // SIGKILLed mid-compile and the next warm tick starts over from
            // scratch (backoffLimit: 0) — so raise the timeout via
            // `DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS` rather than trimming the
            // compile set. See the `warm_job_timeout_seconds` field doc in
            // `config.rs` for the full timing breakdown.
            active_deadline_seconds: Some(config.warm_job_timeout_seconds),
            ..JobSpec::default()
        }),
        ..Job::default()
    }
}

fn job_labels(project_id: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_COMPONENT.into(), COMPONENT_GRAPH_WARM.into());
    labels.insert(LABEL_WARM.into(), "true".into());
    labels.insert(LABEL_PROJECT_ID.into(), sanitize_id(project_id));
    labels
}

fn env_var(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        ..EnvVar::default()
    }
}

/// Sanitise a project id to a DNS-label-safe form for Job names and label
/// values. Mirrors the helper in `djinn-image-controller::build_job`.
pub(crate) fn sanitize_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.len() > 48 {
        out.truncate(48);
    }
    out
}

/// Short form of a uuid v7 used as the Job-name disambiguator (full uuid
/// overruns DNS label budgets when combined with project id + prefix).
pub(crate) fn short_uuid(id: &Uuid) -> String {
    let full = id.simple().to_string();
    full[..12.min(full.len())].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_warm_job_manifest_with_expected_shape() {
        let mut cfg = KubernetesConfig::for_testing();
        cfg.database_url = Some("postgres://djinn@djinn-postgres:5432/djinn".into());
        let job = build_warm_job(
            &cfg,
            "proj-xyz",
            "reg.example:5000/djinn-project-p:abc123",
            None,
        );

        let meta = &job.metadata;
        let name = meta.name.as_deref().expect("name");
        assert!(name.starts_with("djinn-warm-proj-xyz-"), "name: {name}");
        assert_eq!(meta.namespace.as_deref(), Some(cfg.namespace.as_str()));

        let labels = meta.labels.as_ref().expect("labels");
        assert_eq!(
            labels.get(LABEL_COMPONENT).map(String::as_str),
            Some(COMPONENT_GRAPH_WARM)
        );
        assert_eq!(labels.get(LABEL_WARM).map(String::as_str), Some("true"));
        assert_eq!(
            labels.get(LABEL_PROJECT_ID).map(String::as_str),
            Some("proj-xyz")
        );

        let spec = job.spec.as_ref().expect("spec");
        assert_eq!(spec.backoff_limit, Some(0));
        assert_eq!(
            spec.ttl_seconds_after_finished,
            Some(cfg.warm_job_ttl_seconds)
        );
        assert_eq!(
            spec.active_deadline_seconds,
            Some(cfg.warm_job_timeout_seconds)
        );

        let pod = spec.template.spec.as_ref().expect("pod");
        assert_eq!(pod.restart_policy.as_deref(), Some("Never"));
        // Must run as uid 10001 like task-runs to share the /cache cargo target
        // PVC without writing root-owned artifacts workers can't overwrite.
        assert_eq!(
            pod.security_context.as_ref().and_then(|s| s.run_as_user),
            Some(10001),
            "warm pod must run as the worker uid to share the cargo cache safely"
        );
        assert_eq!(
            pod.service_account_name.as_deref(),
            Some(cfg.service_account.as_str())
        );
        assert_eq!(pod.containers.len(), 1);

        // Default config carries no scheduling hints — manifest must be
        // byte-identical to the pre-feature shape. Mirrors the equivalent
        // assertion in job.rs.
        assert!(
            pod.node_selector.is_none(),
            "default config must not set nodeSelector"
        );
        assert!(
            pod.tolerations.is_none(),
            "default config must not set tolerations"
        );

        let container = &pod.containers[0];
        assert_eq!(container.name, "warmer");
        // Warm Pod runs on the per-project devcontainer image — that's
        // where the language indexers (rust-analyzer SCIP etc.) live.
        assert_eq!(
            container.image.as_deref(),
            Some("reg.example:5000/djinn-project-p:abc123")
        );
        // Pod command is a bash wrapper that clones the mirror before execing
        // the warm binary.
        let cmd = container.command.as_ref().expect("command");
        assert_eq!(cmd.len(), 3);
        assert_eq!(cmd[0], "/bin/bash");
        assert_eq!(cmd[1], "-c");
        assert!(cmd[2].contains("git clone"), "bash -c script: {}", cmd[2]);
        // Warm clone must give the coupling index enough history to walk
        // `cursor..HEAD` without a forced unshallow on every warm. Depth
        // 1000 covers the typical case (warm cadence is <100 new commits,
        // so the saved cursor almost always lands in this window). See
        // `cases/plan-a-warm-cargo-base-reuse-validated-working-v0-6-11-0-6-12`
        // and `coupling_index::try_fetch_cursor` for the fallback path
        // when the cursor is older than the clone depth. The substring
        // match has to look for the leading space — bare `--depth 1`
        // would otherwise match the first three chars of `--depth 1000`.
        assert!(
            cmd[2].contains(" --depth 1000"),
            "warm clone must use --depth 1000 so the saved coupling cursor is \
             reachable on a fresh clone: {}",
            cmd[2]
        );
        assert!(
            !cmd[2].contains(" --depth 1 "),
            "warm clone must NOT use --depth 1 (forces an unshallow on every \
             warm): {}",
            cmd[2]
        );
        assert!(cmd[2].contains(WARM_COMMAND_BIN));
        assert!(cmd[2].contains("warm-graph \"proj-xyz\""));
        // JS deps are installed (lockfile-gated) before warming so the TS
        // indexer can resolve workspace-package tsconfig `extends`.
        assert!(
            cmd[2].contains("pnpm-lock.yaml"),
            "bash -c script: {}",
            cmd[2]
        );
        assert!(cmd[2].contains("pnpm install"));
        // The cargo target base is warmed inside `warm-graph` (the worker), where
        // mtimes are normalized to match task-run — NOT in this shell wrapper.
        // The old in-shell `cargo` step gated on a root `Cargo.toml` djinn's
        // `server/` workspace lacks, so it never ran; guard against its return.
        assert!(
            !cmd[2].contains("cargo clippy"),
            "cargo warm must live in the worker, not the warm-Job shell: {}",
            cmd[2]
        );

        let envs: BTreeMap<&str, &str> = container
            .env
            .as_ref()
            .expect("env")
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            envs.get("DJINN_MIRROR_ROOT").copied(),
            Some(MIRROR_MOUNT_DIR)
        );
        assert_eq!(envs.get("DJINN_WARM_PROJECT_ID").copied(), Some("proj-xyz"));
        // DJINN_SERVER_ADDR is intentionally absent — `warm-graph` lives
        // on a disjoint subcommand whose `WorkerDefaultArgs` are not
        // parsed, so any residual envs would only be noise.
        assert!(!envs.contains_key("DJINN_SERVER_ADDR"));
        assert_eq!(
            envs.get("DJINN_PROJECT_ROOT").copied(),
            Some(format!("{WORKSPACE_MOUNT_DIR}/proj-xyz").as_str()),
        );
        // DB env forwarded from KubernetesConfig so the warm Pod shares
        // the server's Postgres target. The worker hard-requires
        // DJINN_DATABASE_URL (postgres cut-over renamed it from
        // DJINN_MYSQL_URL); regression guard for the warm-path miss.
        assert_eq!(
            envs.get("DJINN_DATABASE_URL").copied(),
            Some("postgres://djinn@djinn-postgres:5432/djinn"),
        );
        assert!(!envs.contains_key("DJINN_MYSQL_URL"));

        // Warm cache routing must keep the shared per-project target base as the
        // warm-owned seed with INCREMENTAL compilation enabled (warm == verify ==
        // worker parity) while task-run Pods use private run target dirs.
        assert_eq!(envs.get("CARGO_HOME").copied(), Some("/cache/cargo"));
        assert_eq!(
            envs.get("CARGO_TARGET_DIR").copied(),
            Some("/cache/cargo-target/proj-xyz"),
        );
        // CARGO_INCREMENTAL=1 + RUSTC_WRAPPER="": all djinn build pods share one
        // incremental-on, sccache-off strategy so the warm seed is reusable.
        assert_eq!(envs.get("CARGO_INCREMENTAL").copied(), Some("1"));
        assert_eq!(
            envs.get("RUSTC_WRAPPER").copied(),
            Some(""),
            "warm pod must clear RUSTC_WRAPPER so incremental works"
        );
        assert_eq!(
            envs.get("SCCACHE_DIR").copied(),
            Some("/cache/sccache/proj-xyz"),
        );
        assert_eq!(envs.get("SCCACHE_CACHE_SIZE").copied(), Some("20G"));
        assert_eq!(envs.get("SQLX_OFFLINE").copied(), Some("true"));
        // Fast linker: mold is installed in the devcontainer image; wire it in
        // for the warm build so the warm base is linked (and fingerprinted)
        // identically to the task-run pods that seed from it.
        assert_eq!(
            envs.get("CARGO_BUILD_RUSTFLAGS").copied(),
            Some("-Clink-arg=-fuse-ld=mold"),
        );

        let mounts = container.volume_mounts.as_ref().expect("mounts");
        assert_eq!(mounts.len(), 4, "mirror + workspace + cache + env-config");
        let by_name: BTreeMap<&str, &VolumeMount> =
            mounts.iter().map(|m| (m.name.as_str(), m)).collect();
        let mirror = by_name.get(VOLUME_MIRROR).expect("mirror mount");
        assert_eq!(mirror.mount_path, MIRROR_MOUNT_DIR);
        assert_eq!(mirror.read_only, Some(true));
        let workspace = by_name.get(VOLUME_WORKSPACE).expect("workspace mount");
        assert_eq!(workspace.mount_path, WORKSPACE_MOUNT_DIR);
        assert_eq!(workspace.read_only, Some(false));
        let cache = by_name.get(crate::job::VOLUME_CACHE).expect("cache mount");
        assert_eq!(cache.mount_path, crate::job::CACHE_MOUNT_DIR);
        assert_eq!(cache.read_only, Some(false));
        let env_config_mount = by_name
            .get(crate::env_config::VOLUME_ENV_CONFIG)
            .expect("env-config mount");
        assert_eq!(
            env_config_mount.mount_path,
            crate::env_config::ENV_CONFIG_MOUNT_DIR
        );
        assert_eq!(env_config_mount.read_only, Some(true));

        let volumes = pod.volumes.as_ref().expect("volumes");
        let by_volume_name: BTreeMap<&str, &Volume> =
            volumes.iter().map(|v| (v.name.as_str(), v)).collect();
        let mirror_v = by_volume_name.get(VOLUME_MIRROR).expect("mirror volume");
        let pvc = mirror_v.persistent_volume_claim.as_ref().expect("pvc");
        assert_eq!(pvc.claim_name, cfg.mirror_pvc);
        assert_eq!(pvc.read_only, Some(true));
        let workspace_v = by_volume_name
            .get(VOLUME_WORKSPACE)
            .expect("workspace volume");
        assert!(
            workspace_v.empty_dir.is_some(),
            "workspace must be emptyDir"
        );
        let cache_v = by_volume_name
            .get(crate::job::VOLUME_CACHE)
            .expect("cache volume");
        let cache_pvc = cache_v
            .persistent_volume_claim
            .as_ref()
            .expect("cache volume is a PVC source");
        assert_eq!(cache_pvc.claim_name, cfg.cache_pvc);
        assert_eq!(cache_pvc.read_only, Some(false));
        let env_v = by_volume_name
            .get(crate::env_config::VOLUME_ENV_CONFIG)
            .expect("env-config volume");
        let cm_src = env_v
            .config_map
            .as_ref()
            .expect("env-config volume is a ConfigMap source");
        assert_eq!(cm_src.name, "djinn-env-proj-xyz");
        assert_eq!(
            cm_src.optional,
            Some(true),
            "env-config CM must be optional so Pods start pre-P6 when the CM doesn't exist yet"
        );

        // Resource requests/limits from `warm_*` config knobs (Gap 4) —
        // without these the warm Pod runs unbounded and SCIP indexers
        // can spike CPU/memory under the kubelet's nose.
        let resources = container
            .resources
            .as_ref()
            .expect("warm container.resources set");
        let requests = resources.requests.as_ref().expect("requests set");
        assert_eq!(
            requests.get("cpu").map(|q| q.0.as_str()),
            Some(cfg.warm_cpu_request.as_str())
        );
        assert_eq!(
            requests.get("memory").map(|q| q.0.as_str()),
            Some(cfg.warm_memory_request.as_str())
        );
        let limits = resources.limits.as_ref().expect("limits set");
        assert_eq!(
            limits.get("cpu").map(|q| q.0.as_str()),
            Some(cfg.warm_cpu_limit.as_str())
        );
        assert_eq!(
            limits.get("memory").map(|q| q.0.as_str()),
            Some(cfg.warm_memory_limit.as_str())
        );
        // Defaults pin the documented values. Memory limit bumped 4Gi → 6Gi to
        // cover the added test-compile warm pass (--all-targets test codegen).
        assert_eq!(cfg.warm_cpu_request, "1");
        assert_eq!(cfg.warm_cpu_limit, "2");
        assert_eq!(cfg.warm_memory_request, "2Gi");
        assert_eq!(cfg.warm_memory_limit, "6Gi");
    }

    #[test]
    fn sanitize_id_lowercases_and_maps_disallowed_chars() {
        assert_eq!(sanitize_id("Proj_ABC/xyz"), "proj-abc-xyz");
    }

    /// Warm Pods must inherit the same scheduling hints as task-runs —
    /// otherwise they'd land on a different pool and the canonical-graph
    /// cache they pre-populate wouldn't be reused by the task-run that
    /// adopts it. The config struct carries one shared set of hints for
    /// exactly this reason; this test guards the wiring through to the
    /// warm-Job PodSpec.
    #[test]
    fn warm_pod_scheduling_propagates_from_config() {
        let mut cfg = KubernetesConfig::for_testing();
        cfg.node_selector
            .insert("workload-type".into(), "djinn".into());
        cfg.tolerations.push(Toleration {
            key: Some("workload-type".into()),
            operator: Some("Equal".into()),
            value: Some("djinn".into()),
            effect: Some("NoSchedule".into()),
            ..Toleration::default()
        });

        let job = build_warm_job(
            &cfg,
            "proj-xyz",
            "reg.example:5000/djinn-project-p:abc123",
            None,
        );
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec set");

        let ns = pod.node_selector.as_ref().expect("nodeSelector set");
        assert_eq!(ns.get("workload-type").map(String::as_str), Some("djinn"));

        let tols = pod.tolerations.as_ref().expect("tolerations set");
        assert_eq!(tols.len(), 1);
        assert_eq!(tols[0].key.as_deref(), Some("workload-type"));
        assert_eq!(tols[0].effect.as_deref(), Some("NoSchedule"));
    }
}
