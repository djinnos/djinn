//! Pure `Job` manifest builder for the standalone SCIP (semantic) index run.
//!
//! # Why this Job exists at all
//!
//! One graph-warm Job used to do everything: a cargo warm-base compile, the
//! rust-analyzer SCIP index, then the graph build. A complete production warm
//! measured 90m42s end to end, of which the SCIP index alone was **58m43s**
//! (~65%).
//!
//! That phase is *structurally* single-threaded. rust-analyzer's
//! `StaticIndex::compute` is a bare `for module in work` loop with no rayon;
//! the measured parallel window was 20s against a 368s serial window (94%
//! serial), and upstream issue rust-lang/rust-analyzer#18140 is open and
//! maintainer-confirmed. So the warm Job reserved a full build slot sized for a
//! 4-core parallel cargo build and then spent roughly two thirds of its life
//! using one core.
//!
//! # Why resizing the warm Job cannot fix it
//!
//! [`djinn_runtime::BuildSlotWeight::for_millicores`] is
//! `millicores.div_ceil(slot_millicores).max(1)`. **Any nonzero CPU request
//! costs at least one whole build slot.** Dropping the warm request from 2 CPU
//! to 500m changes the charged weight not at all. The only lever that returns
//! capacity is for the SCIP work to acquire *no build lease*, which means it
//! must not be a `LeaseIdentity::GraphWarm` — i.e. it must be its own Job.
//! [`crate::graph_warmer`] is the only producer of `GraphWarm` lease
//! identities and this module is deliberately not reachable from it.
//!
//! # Why the Job must still declare itself
//!
//! A Job that acquires no lease *and* is not labelled protected is invisible
//! CPU: it burns real cores that no cap can account for. Proposal `8ixk`
//! ("Node-keyed build slot pools") derives each node's cap as
//! `floor((allocatable_mcpu - protected_mcpu) / cores_per_slot_mcpu)` with
//! `cores_per_slot` defaulting to 2800, where `protected_mcpu` is the summed
//! CPU request of Pods carrying [`LABEL_CAPACITY_RESERVED`]. So this Job
//! carries that label, its request folds into `protected_mcpu`, and the derived
//! cap stays honest.
//!
//! On the current 12-core VPS with 4200m of already-protected requests:
//!
//! ```text
//! today                floor((12000 - 4200) / 2800) = floor(2.78) = 2
//! with this Job @1000m floor((12000 - 5200) / 2800) = floor(2.43) = 2   (unchanged)
//! ```
//!
//! The budget before the derived cap drops to one slot is a request of
//! **2200m**: `floor((12000 - 4200 - 2200) / 2800) = floor(2.0) = 2`, while
//! 2201m tips it over.
//! [`SCIP_PROTECTED_REQUEST_CEILING_MILLICORES`] pins that ceiling, and
//! `scip_cpu_request_stays_inside_the_8ixk_protected_budget` proves the
//! rendered request respects it *by recomputing the formula*, not by comparing
//! against a copied constant.
//!
//! # What it produces and who consumes it
//!
//! The SCIP indexer already writes content-addressed artifacts into
//! `$XDG_CACHE_HOME/djinn/scip-indexer` (`djinn_graph::scip_indexer::cache`),
//! and [`crate::job::cache_env_vars`] points `XDG_CACHE_HOME` at the shared
//! `/cache` PVC. So this Job's output is consumed with **no pipeline change**:
//! the next warm Job's `execute_plan_with_cache` observes a `CachedHit` for
//! every workspace whose cache key still matches and skips the re-index.
//!
//! Because the cache key is content-derived (source/config/lockfile hashes),
//! not commit-derived, a commit touching only workspace A leaves workspace B's
//! key intact and B still hits. A miss is not a regression — it is exactly
//! today's behaviour, the warm Job re-indexes inline.
//!
//! # Degradation
//!
//! This Job never publishes a graph. It only fills a cache. If it fails, is
//! skipped, or never runs, the warm path is bit-for-bit what it is today:
//! `ensure_canonical_graph` re-runs the indexers inline, and its existing
//! guards (the `node_count == 0` early return that refuses to write an empty
//! blob, and per-workspace salvage from the previous artifact) continue to
//! hold. There is no code path by which a missing SCIP Job can degrade the
//! served graph to empty — the worst case is a slower warm.

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, PersistentVolumeClaimVolumeSource, PodSpec,
    PodTemplateSpec, ResourceRequirements, Toleration, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

use crate::config::KubernetesConfig;
use crate::warm_job::{
    LABEL_COMPONENT, MIRROR_MOUNT_DIR, VOLUME_MIRROR, VOLUME_WORKSPACE, WARM_COMMAND_BIN,
    WORKSPACE_MOUNT_DIR, sanitize_id,
};

/// Label key identifying a standalone SCIP-index Job.
pub const LABEL_SCIP_INDEX: &str = "djinn.app/scip-index";
/// Label key for the project id a SCIP-index Job targets. Deliberately the same
/// key the warm Job uses so one selector dimension covers both populations.
pub const LABEL_PROJECT_ID: &str = crate::warm_job::LABEL_PROJECT_ID;
/// `djinn.app/component` value written on SCIP-index resources.
pub const COMPONENT_SCIP_INDEX: &str = "scip-index";

/// Proposal `8ixk`'s protected-workload label.
///
/// A Pod carrying this label has its scheduler-effective CPU request folded
/// into `protected_mcpu` and subtracted from the capacity build slots are
/// derived from. Every first-party workload whose CPU must not be handed out as
/// build capacity carries it. This Job takes no build lease, so without this
/// label its CPU would be accounted by nothing at all.
pub const LABEL_CAPACITY_RESERVED: &str = "djinn.io/capacity-reserved";

/// Annotation recording which repository revision this index run covers. The
/// scheduler in [`crate::scip_schedule`] reads it back off retained Jobs to
/// decide whether the head has advanced since the last successful index.
pub const ANNOTATION_SCIP_REVISION: &str = "djinn.app/scip-revision";

/// Env key under which the enclosing Job's `activeDeadlineSeconds` is projected
/// into the Pod.
///
/// Named for the reader, not the writer: `djinn_graph::scip_indexer::budget`
/// and `djinn_agent_worker::warm_step_budget` both look up this exact string to
/// find "the deadline that will kill the Pod I am in". A SCIP-specific key
/// would render fine, pass every manifest test, and be read by nobody.
pub const SCIP_JOB_DEADLINE_ENV: &str = "DJINN_WARM_JOB_DEADLINE_SECONDS";

/// Container name of the SCIP indexer. Distinct from the warm Job's `warmer`
/// so [`crate::build_resources::apply_resolved_resources`] — which targets
/// `worker`/`warmer` — cannot silently retarget this Pod with warm overrides.
pub const SCIP_CONTAINER_NAME: &str = "scip-indexer";

/// Measured peak RSS of the SCIP phase on the production workspace, in bytes:
/// **10.0 GB**.
///
/// This is the number the rendered memory limit is regression-tested against.
/// It is not a guess and not a comment: `scip_memory_limit_covers_the_measured_peak`
/// fails the build if the rendered limit ever drops below it. The warm Job's
/// `warm_memory_limit` default is `6Gi` — below this peak — and production
/// survives only because it overrides to 16Gi out of band. That is exactly the
/// arrangement this constant exists to prevent repeating.
pub const MEASURED_SCIP_PEAK_MEMORY_BYTES: i64 = 10_000_000_000;

/// Measured wall-clock of the SCIP phase inside the combined warm Job:
/// 58m43s = 3523s.
pub const MEASURED_SCIP_PHASE_SECONDS: i64 = 3_523;

pub use crate::capacity::SCIP_PROTECTED_REQUEST_CEILING_MILLICORES;

/// Deterministic Job name for one `(project, revision)` index run.
///
/// Deterministic so a lost create response cannot produce two Jobs for the same
/// revision, and so the retained-Job inventory the scheduler reads is keyed by
/// the thing it actually decides on.
#[must_use]
pub fn scip_index_job_name(project_id: &str, revision: &str) -> String {
    let project = sanitize_id(project_id);
    let rev: String = revision
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .flat_map(char::to_lowercase)
        .collect();
    format!("djinn-scip-{project}-{rev}")
}

/// Labels every SCIP-index resource carries.
fn job_labels(config: &KubernetesConfig, project_id: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([
        (LABEL_COMPONENT.into(), COMPONENT_SCIP_INDEX.into()),
        (LABEL_SCIP_INDEX.into(), "true".into()),
        (LABEL_PROJECT_ID.into(), sanitize_id(project_id)),
        // 8ixk: this Job takes no build lease, so this label is the ONLY thing
        // that makes its CPU visible to the capacity derivation.
        (LABEL_CAPACITY_RESERVED.into(), "true".into()),
    ]);
    // SCIP joins Kueue: rust-analyzer indexing is genuinely CPU-heavy, so
    // excluding it would make the ClusterQueue under-count the very load the
    // quota exists to bound. Stamped into the one map cloned into both the Job
    // metadata and the Pod template. No-op unless `config.kueue_armed`.
    config.apply_kueue_build_object_labels(crate::config::KueueQueueKind::Scip, &mut labels);
    labels
}

fn env_var(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        ..EnvVar::default()
    }
}

/// Build the Job manifest for one standalone SCIP index run at `revision`.
///
/// The Pod clones the project's mirror, installs JS dependencies when a
/// lockfile is present (scip-typescript cannot resolve workspace-package
/// `tsconfig` `extends` without `node_modules`), then execs
/// `djinn-agent-worker scip-index <project_id>`, which runs the semantic pass
/// only and leaves its artifacts in the shared content-addressed SCIP cache.
///
/// It renders **no** launcher sidecar, **no** warm lease gate, and **no** lease
/// annotations — see `scip_job_takes_no_build_lease_surface`.
pub fn build_scip_index_job(
    config: &KubernetesConfig,
    project_id: &str,
    project_image_tag: &str,
    revision: &str,
    policy: Option<&djinn_stack::environment::CargoCachePolicy>,
    js_workspace_roots: &[String],
) -> Job {
    let sanitized_project = sanitize_id(project_id);
    let job_name = scip_index_job_name(project_id, revision);
    let labels = job_labels(config, project_id);
    let annotations =
        BTreeMap::from([(ANNOTATION_SCIP_REVISION.to_string(), revision.to_string())]);

    let project_root = format!("{WORKSPACE_MOUNT_DIR}/{sanitized_project}");
    let mirror_path = format!("{MIRROR_MOUNT_DIR}/{project_id}.git");

    // Deliberately the same clone preamble as the warm Job (umask, the
    // GIT_CONFIG_SYSTEM safe.directory file, --depth 1000 --single-branch,
    // lockfile-gated JS install). The two Pods index the same tree; a
    // divergence here is a divergence in what gets indexed.
    let cmd = format!(
        r#"set -euo pipefail
umask 0002
export GIT_CONFIG_SYSTEM={WORKSPACE_MOUNT_DIR}/.djinn-gitconfig
unset GIT_CONFIG_NOSYSTEM
printf '[safe]\n\tdirectory = *\n' > "$GIT_CONFIG_SYSTEM"
UPSTREAM_URL="$(git -C "{mirror_path}" config remote.origin.url)"
git clone --depth 1000 --single-branch "$UPSTREAM_URL" "{project_root}"
cd "{project_root}"{js_install}
exec {bin} scip-index "{project_id}"
"#,
        mirror_path = mirror_path,
        project_root = project_root,
        bin = WARM_COMMAND_BIN,
        project_id = project_id,
        js_install = crate::js_install::js_install_preamble(&project_root, js_workspace_roots),
    );

    let mut env = vec![
        env_var("DJINN_MIRROR_ROOT", MIRROR_MOUNT_DIR),
        env_var("DJINN_SCIP_PROJECT_ID", project_id),
        env_var("DJINN_PROJECT_ROOT", &project_root),
        env_var("DJINN_SCIP_REVISION", revision),
        env_var("RUST_LOG", "info,djinn=debug"),
        // THE KEY IS THE CONTRACT, AND THE READER NAMED IT.
        //
        // `djinn_graph::scip_indexer::budget` bounds each indexer's timeout by
        // the enclosing Job's deadline, and it reads exactly
        // `DJINN_WARM_JOB_DEADLINE_SECONDS`. The value is this Job's own
        // `activeDeadlineSeconds`, not the warm Job's — only the *name* is
        // shared, because the budget's question ("what deadline will kill the
        // Pod I am running in?") is the same in both Pods.
        //
        // This originally rendered as `DJINN_SCIP_JOB_DEADLINE_SECONDS`, which
        // nothing reads. With the key unread the budget falls back to its
        // unenclosed 4h ceiling while the kubelet kills this Pod at
        // `scip_job_timeout_seconds` — turning an observable `timed_out`
        // indexer status into an unexplained SIGKILL with no artifacts written.
        env_var(
            SCIP_JOB_DEADLINE_ENV,
            &config.scip_job_timeout_seconds.to_string(),
        ),
    ];
    if let Some(url) = config.database_url.as_deref() {
        env.push(env_var("DJINN_DATABASE_URL", url));
    }

    // CACHE PARITY IS LOAD-BEARING, AND IT IS PARITY WITH THE *WARM* POD.
    //
    // These vars are derived from `config.warm_cpu_limit`, NOT from this Pod's
    // own `scip_cpu_limit`. That looks wrong and is not:
    //
    //   * `CARGO_BUILD_RUSTFLAGS` embeds `-Clink-arg=-Wl,--threads=N` with N
    //     derived from the limit. RUSTFLAGS feed rustc's `-C metadata`, so a
    //     `--threads=2` render would give every compilation unit a different
    //     fingerprint from the warm base and discard 100% of it.
    //   * `CARGO_TARGET_DIR` is keyed `.../mold-jobs-N` from the same limit, so
    //     a divergent N would fork a second, cold target base on the PVC.
    //   * `CARGO_INCREMENTAL=1` / `RUSTC_WRAPPER=""` are the shared compile
    //     strategy every djinn build pod must agree on (see the doc comment on
    //     [`crate::job::warm_cache_env_vars`]); cargo folds both into its
    //     fingerprints.
    //
    // `scip_job_cache_env_matches_the_warm_job_render` asserts this against the
    // warm Job's actual rendered env, so passing `scip_cpu_limit` here fails.
    env.extend(crate::job::warm_cache_env_vars(
        project_id,
        &config.warm_cpu_limit,
        policy,
    ));

    let container = Container {
        name: SCIP_CONTAINER_NAME.to_string(),
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
            VolumeMount {
                name: crate::job::VOLUME_CACHE.to_string(),
                mount_path: crate::job::CACHE_MOUNT_DIR.to_string(),
                read_only: Some(false),
                ..VolumeMount::default()
            },
            crate::env_config::env_config_volume_mount(),
        ]),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(config.scip_cpu_request.clone())),
                (
                    "memory".to_string(),
                    Quantity(config.scip_memory_request.clone()),
                ),
            ])),
            limits: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(config.scip_cpu_limit.clone())),
                (
                    "memory".to_string(),
                    Quantity(config.scip_memory_limit.clone()),
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
        termination_grace_period_seconds: Some(config.warm_job_termination_grace_period_seconds),
        // Same uid rationale as the warm Pod: this Pod CREATES content in the
        // shared `/cache` tree (the SCIP artifact cache, and whatever the
        // indexers spill into the cargo target base), and chmod/chown/utimes
        // are governed by ownership alone. It renders no launcher and no broker
        // socket, so CHILD_UID carries no security consequence here either.
        security_context: Some(k8s_openapi::api::core::v1::PodSecurityContext {
            run_as_user: Some(i64::from(crate::launcher::CHILD_UID)),
            run_as_group: Some(i64::from(crate::launcher::ARTIFACT_GID)),
            ..crate::launcher::pod_security_context()
        }),
        ..PodSpec::default()
    };

    Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: Some(config.namespace.clone()),
            labels: Some(labels.clone()),
            annotations: Some(annotations.clone()),
            ..ObjectMeta::default()
        },
        spec: Some(JobSpec {
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    annotations: Some(annotations),
                    ..ObjectMeta::default()
                }),
                spec: Some(pod_spec),
            },
            backoff_limit: Some(0),
            // Kueue create-then-admit; `None` when disarmed. See
            // `KubernetesConfig::kueue_job_suspend`.
            suspend: config.kueue_job_suspend(),
            // Retained deliberately longer than the index interval: the
            // retained succeeded-Job inventory IS the change-detection ledger
            // read by [`crate::scip_schedule`]. See
            // `scip_job_ttl_outlives_the_index_interval`.
            ttl_seconds_after_finished: Some(config.scip_job_ttl_seconds),
            active_deadline_seconds: Some(config.scip_job_timeout_seconds),
            ..JobSpec::default()
        }),
        ..Job::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: i64 = 1 << 30;

    /// Exact integer millicores from a rendered CPU `Quantity`. Panics rather
    /// than rounding: a render-shape change must be noticed, not absorbed.
    fn cpu_millicores(q: &str) -> i64 {
        match q.strip_suffix('m') {
            Some(m) => m
                .parse()
                .unwrap_or_else(|_| panic!("non-integer millicore quantity: {q:?}")),
            None => {
                q.parse::<i64>()
                    .unwrap_or_else(|_| panic!("non-integer whole-core quantity: {q:?}"))
                    * 1000
            }
        }
    }

    fn memory_bytes(q: &str) -> i64 {
        for (suffix, mult) in [("Gi", GIB), ("Mi", 1 << 20), ("Ki", 1 << 10)] {
            if let Some(n) = q.strip_suffix(suffix) {
                return n
                    .parse::<i64>()
                    .unwrap_or_else(|_| panic!("non-integer memory quantity: {q:?}"))
                    * mult;
            }
        }
        q.parse::<i64>()
            .unwrap_or_else(|_| panic!("unrecognized memory quantity: {q:?}"))
    }

    fn job(cfg: &KubernetesConfig) -> Job {
        build_scip_index_job(
            cfg,
            "proj-xyz",
            "reg.example:5000/p:abc",
            "deadbeef1234567890",
            None,
            &[],
        )
    }

    fn container_of(job: &Job) -> &Container {
        &job.spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec")
            .containers[0]
    }

    fn env_of(job: &Job) -> BTreeMap<String, String> {
        container_of(job)
            .env
            .as_ref()
            .expect("env")
            .iter()
            .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
            .collect()
    }

    fn resources_of(job: &Job) -> &ResourceRequirements {
        container_of(job).resources.as_ref().expect("resources")
    }

    fn request(job: &Job, key: &str) -> String {
        resources_of(job)
            .requests
            .as_ref()
            .expect("requests")
            .get(key)
            .expect("key")
            .0
            .clone()
    }

    fn limit(job: &Job, key: &str) -> String {
        resources_of(job)
            .limits
            .as_ref()
            .expect("limits")
            .get(key)
            .expect("key")
            .0
            .clone()
    }

    #[test]
    fn builds_scip_index_job_with_expected_shape() {
        let mut cfg = KubernetesConfig::for_testing();
        cfg.database_url = Some("postgres://djinn@djinn-postgres:5432/djinn".into());
        cfg.warm_job_termination_grace_period_seconds = 47;
        let job = job(&cfg);

        assert_eq!(
            job.metadata.name.as_deref(),
            Some("djinn-scip-proj-xyz-deadbeef1234")
        );
        assert_eq!(
            job.metadata.namespace.as_deref(),
            Some(cfg.namespace.as_str())
        );

        let labels = job.metadata.labels.as_ref().expect("labels");
        assert_eq!(
            labels.get(LABEL_COMPONENT).map(String::as_str),
            Some(COMPONENT_SCIP_INDEX)
        );
        assert_eq!(
            labels.get(LABEL_SCIP_INDEX).map(String::as_str),
            Some("true")
        );
        assert_eq!(
            labels.get(LABEL_PROJECT_ID).map(String::as_str),
            Some("proj-xyz")
        );

        let annotations = job.metadata.annotations.as_ref().expect("annotations");
        assert_eq!(
            annotations
                .get(ANNOTATION_SCIP_REVISION)
                .map(String::as_str),
            Some("deadbeef1234567890"),
            "the scheduler reads the FULL revision back off this annotation; a \
             truncated value would make head-advance detection compare prefixes"
        );

        let spec = job.spec.as_ref().expect("spec");
        assert_eq!(spec.backoff_limit, Some(0));
        assert_eq!(
            spec.active_deadline_seconds,
            Some(cfg.scip_job_timeout_seconds)
        );
        assert_eq!(
            spec.template
                .spec
                .as_ref()
                .unwrap()
                .termination_grace_period_seconds,
            Some(cfg.warm_job_termination_grace_period_seconds),
        );

        let cmd = &container_of(&job).command.as_ref().expect("command")[2];
        assert!(cmd.contains("scip-index \"proj-xyz\""), "{cmd}");
        assert!(
            !cmd.contains("warm-graph"),
            "the SCIP Job must not invoke the combined warm pipeline: {cmd}"
        );
        assert!(cmd.contains(" --depth 1000"), "{cmd}");
        assert!(cmd.contains("pnpm-lock.yaml"), "{cmd}");

        let env = env_of(&job);
        assert_eq!(
            env.get("DJINN_SCIP_REVISION").map(String::as_str),
            Some("deadbeef1234567890")
        );
        assert_eq!(
            env.get("DJINN_PROJECT_ROOT").map(String::as_str),
            Some("/workspace/proj-xyz")
        );
        assert_eq!(
            env.get("DJINN_DATABASE_URL").map(String::as_str),
            Some("postgres://djinn@djinn-postgres:5432/djinn")
        );
    }

    /// **The 16Gi floor.**
    ///
    /// Measured peak RSS for the SCIP phase is 10.0 GB. The warm Job's
    /// `warm_memory_limit` default is `6Gi`, and production only survives
    /// because an out-of-band override raises it to 16Gi. This Job sets the
    /// real figure explicitly, and this test fails the build if the rendered
    /// limit ever drops below what was measured — a default, a comment, or an
    /// operator override is not an acceptable substitute.
    #[test]
    fn scip_memory_limit_covers_the_measured_peak() {
        let cfg = KubernetesConfig::for_testing();
        let rendered = limit(&job(&cfg), "memory");
        assert_eq!(rendered, "16Gi");
        assert!(
            memory_bytes(&rendered) >= MEASURED_SCIP_PEAK_MEMORY_BYTES,
            "rendered SCIP memory limit {rendered} = {} bytes is below the \
             MEASURED peak of {} bytes; the Pod will be OOMKilled mid-index",
            memory_bytes(&rendered),
            MEASURED_SCIP_PEAK_MEMORY_BYTES,
        );
        // Not merely "some large number": a 6Gi render (the warm default this
        // Job must not inherit) must fail this assertion.
        assert!(
            memory_bytes("6Gi") < MEASURED_SCIP_PEAK_MEMORY_BYTES,
            "the measured peak must sit ABOVE the warm default, or this test \
             would pass for a Pod that OOMs"
        );
        assert!(
            memory_bytes(&request(&job(&cfg), "memory")) <= memory_bytes(&rendered),
            "memory request must not exceed its limit"
        );
    }

    /// **The 8ixk protected-request budget.**
    ///
    /// Recomputes 8ixk's derivation from its inputs rather than comparing the
    /// rendered request against a copied number: with this Job's request folded
    /// into `protected_mcpu`, the derived cap must still be
    /// the named Item 1 capacity fixture.
    #[test]
    fn scip_cpu_request_stays_inside_the_8ixk_protected_budget() {
        let cfg = KubernetesConfig::for_testing();
        let requested = cpu_millicores(&request(&job(&cfg), "cpu"));
        assert_eq!(requested, 1_000, "documented sizing is a 1 CPU request");

        use crate::capacity::{
            CapacityOutcome, CpuMillicores, FailSafeCapacity, SCIP_CAPACITY_FIXTURE,
            SCIP_FIXTURE_COMPILE_SLOTS, derive as derive_capacity,
        };
        let derive = |extra_protected: i64| -> i64 {
            let mut inputs = SCIP_CAPACITY_FIXTURE;
            inputs.protected =
                CpuMillicores::new(inputs.protected.get() + extra_protected).unwrap();
            match derive_capacity(
                inputs,
                FailSafeCapacity {
                    pods: 1,
                    compile_slots: 1,
                },
            ) {
                CapacityOutcome::Derived(value) => value.compile_slots,
                CapacityOutcome::Conservative { capacity, .. } => capacity.compile_slots,
            }
        };

        assert_eq!(
            derive(0),
            SCIP_FIXTURE_COMPILE_SLOTS,
            "8ixk baseline on this node"
        );
        assert_eq!(
            derive(requested),
            SCIP_FIXTURE_COMPILE_SLOTS,
            "rendering this Job at {requested}m must not lower the derived \
             build-slot cap from {SCIP_FIXTURE_COMPILE_SLOTS}",
        );

        // The ceiling is the solved boundary, not an opinion: at exactly the
        // ceiling the cap holds; one millicore past it, the deployment loses a
        // build slot.
        assert_eq!(SCIP_PROTECTED_REQUEST_CEILING_MILLICORES, 2_200);
        assert_eq!(
            derive(SCIP_PROTECTED_REQUEST_CEILING_MILLICORES),
            SCIP_FIXTURE_COMPILE_SLOTS
        );
        assert_eq!(
            derive(SCIP_PROTECTED_REQUEST_CEILING_MILLICORES + 1),
            SCIP_FIXTURE_COMPILE_SLOTS - 1,
            "one millicore over the ceiling must provably cost a whole slot"
        );
        assert!(
            requested <= SCIP_PROTECTED_REQUEST_CEILING_MILLICORES,
            "SCIP CPU request {requested}m exceeds the {SCIP_PROTECTED_REQUEST_CEILING_MILLICORES}m \
             budget and costs the deployment a build slot"
        );
        assert!(cpu_millicores(&limit(&job(&cfg), "cpu")) >= requested);
    }

    /// The label without which this Job is invisible CPU.
    ///
    /// It takes no build lease. If it is also not labelled protected, its cores
    /// are accounted by nothing — precisely the oversubscription 8ixk exists to
    /// prevent. Asserted on the Pod template, not just the Job: 8ixk selects
    /// *Pods*.
    #[test]
    fn scip_pod_is_labelled_capacity_reserved() {
        let cfg = KubernetesConfig::for_testing();
        let job = job(&cfg);
        for (where_, labels) in [
            ("job", job.metadata.labels.as_ref()),
            (
                "pod template",
                job.spec
                    .as_ref()
                    .and_then(|s| s.template.metadata.as_ref())
                    .and_then(|m| m.labels.as_ref()),
            ),
        ] {
            assert_eq!(
                labels
                    .and_then(|l| l.get(LABEL_CAPACITY_RESERVED))
                    .map(String::as_str),
                Some("true"),
                "{where_} must carry {LABEL_CAPACITY_RESERVED}; 8ixk sums \
                 protected CPU from Pod labels, and this Job holds no lease"
            );
        }
        // The warm Job, by contrast, pays for its CPU with a build lease and is
        // deliberately NOT protected — proving this assertion reads the render.
        let warm =
            crate::warm_job::build_warm_job(&cfg, "proj-xyz", "deadbeef", "img:t", None, &[]);
        assert!(
            !warm
                .metadata
                .labels
                .as_ref()
                .is_some_and(|l| l.contains_key(LABEL_CAPACITY_RESERVED)),
            "the warm Job is charged a build slot; labelling it protected would \
             double-count its CPU"
        );
    }

    /// This Job must acquire no build lease. It cannot be charged a
    /// `BuildSlotWeight` because it renders none of the surface a leased warm
    /// Job carries: no fencing token, no gate ConfigMap volume, no init gate,
    /// no launcher sidecar.
    #[test]
    fn scip_job_takes_no_build_lease_surface() {
        let cfg = KubernetesConfig::for_testing();
        let job = job(&cfg);
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod spec");

        assert!(
            pod.init_containers.as_ref().is_none_or(|c| c.is_empty()),
            "a lease gate init container implies a lease this Job never takes"
        );
        let volumes: Vec<&str> = pod
            .volumes
            .iter()
            .flatten()
            .map(|v| v.name.as_str())
            .collect();
        assert!(!volumes.contains(&crate::warm_job::VOLUME_WARM_GATE));
        assert!(!volumes.contains(&crate::launcher::VOLUME_LAUNCHER_IPC));
        let names: Vec<&str> = pod
            .containers
            .iter()
            .chain(pod.init_containers.iter().flatten())
            .map(|c| c.name.as_str())
            .collect();
        assert!(!names.contains(&crate::launcher::LAUNCHER_CONTAINER_NAME));

        let annotations = job.metadata.annotations.as_ref().expect("annotations");
        for key in [
            crate::warm_job::ANNOTATION_FENCING_TOKEN,
            crate::warm_job::ANNOTATION_WARM_REQUEST_ID,
        ] {
            assert!(
                !annotations.contains_key(key),
                "unexpected lease annotation {key}"
            );
        }
        // The warm-Job selector must not sweep this Job into the warm
        // population: a warmer that saw it would treat it as an in-flight warm.
        assert_ne!(
            job.metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(crate::warm_job::LABEL_WARM)),
            Some(&"true".to_string()),
        );
    }

    /// **Cache/compile parity.**
    ///
    /// Every fingerprint-bearing cache var must be byte-identical to the warm
    /// Job's render. `CARGO_BUILD_RUSTFLAGS` carries `-Wl,--threads=N` derived
    /// from the CPU limit and feeds rustc's `-C metadata`; a divergent N gives
    /// all 2797 compilation units different fingerprints and discards the whole
    /// warm base. `CARGO_TARGET_DIR` is keyed by the same N and would fork a
    /// second cold base on the PVC.
    #[test]
    fn scip_job_cache_env_matches_the_warm_job_render() {
        let mut cfg = KubernetesConfig::for_testing();
        // Non-default, and deliberately DIFFERENT from the warm limit, so a
        // render that keyed the cache off `scip_cpu_limit` diverges visibly.
        cfg.warm_cpu_limit = "6".into();
        cfg.scip_cpu_limit = "2".into();

        let warm =
            crate::warm_job::build_warm_job(&cfg, "proj-xyz", "deadbeef", "img:t", None, &[]);
        let warm_env: BTreeMap<String, String> = warm
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("warm pod")
            .containers[0]
            .env
            .as_ref()
            .expect("warm env")
            .iter()
            .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
            .collect();
        let scip_env = env_of(&job(&cfg));

        for key in [
            "CARGO_BUILD_RUSTFLAGS",
            "CARGO_TARGET_DIR",
            "CARGO_INCREMENTAL",
            "RUSTC_WRAPPER",
            "CARGO_HOME",
            "SCCACHE_DIR",
            "XDG_CACHE_HOME",
            "SQLX_OFFLINE",
        ] {
            assert_eq!(
                scip_env.get(key),
                warm_env.get(key),
                "{key} diverges between the SCIP Job and the warm Job; warm and \
                 SCIP pods must use the SAME compile strategy or cargo \
                 fingerprints (which fold in RUSTFLAGS, CARGO_INCREMENTAL and \
                 the rustc wrapper) discard the shared warm base"
            );
        }
        assert_eq!(
            scip_env.get("CARGO_INCREMENTAL").map(String::as_str),
            Some("1")
        );
        assert_eq!(scip_env.get("RUSTC_WRAPPER").map(String::as_str), Some(""));
        assert_eq!(
            scip_env.get("CARGO_BUILD_RUSTFLAGS").map(String::as_str),
            Some("-Clink-arg=-fuse-ld=mold -Clink-arg=-Wl,--threads=6"),
            "keyed off warm_cpu_limit (6), NOT scip_cpu_limit (2)"
        );
        assert_eq!(
            scip_env.get("CARGO_TARGET_DIR").map(String::as_str),
            Some("/cache/cargo-target/proj-xyz/mold-jobs-6"),
        );
        // XDG_CACHE_HOME is what routes the content-addressed SCIP artifact
        // cache onto the shared PVC. Without it this Job produces nothing the
        // warm Job can consume, and the whole split is inert.
        assert_eq!(
            scip_env.get("XDG_CACHE_HOME").map(String::as_str),
            Some("/cache/xdg/proj-xyz"),
        );
    }

    /// The retained succeeded-Job set is the change-detection ledger, so it has
    /// to outlive the cadence it gates. A TTL shorter than the interval makes
    /// every tick look like "never indexed".
    #[test]
    fn scip_job_ttl_outlives_the_index_interval() {
        let cfg = KubernetesConfig::for_testing();
        assert!(
            i64::from(cfg.scip_job_ttl_seconds) > cfg.scip_index_interval_seconds as i64,
            "ttl {} must exceed the {}s index interval, or the retained-Job \
             ledger expires before the next decision reads it",
            cfg.scip_job_ttl_seconds,
            cfg.scip_index_interval_seconds,
        );
    }

    /// The deadline must clear the measured phase cost with margin, or the Pod
    /// is SIGKILLed mid-index and the cache is never written.
    #[test]
    fn scip_deadline_clears_the_measured_phase_cost() {
        let cfg = KubernetesConfig::for_testing();
        assert!(
            cfg.scip_job_timeout_seconds >= MEASURED_SCIP_PHASE_SECONDS * 2,
            "scip_job_timeout_seconds {} leaves no margin over the measured \
             {MEASURED_SCIP_PHASE_SECONDS}s SCIP phase",
            cfg.scip_job_timeout_seconds,
        );
    }

    /// The deadline must reach the indexer budget, which means it must be
    /// projected under the key that budget reads.
    ///
    /// A rendered-but-unread key is indistinguishable from a correct one in the
    /// manifest, and its only symptom is a Pod the kubelet kills partway
    /// through an index that believed it had four hours.
    #[test]
    fn scip_deadline_is_projected_under_the_key_the_indexer_budget_reads() {
        let cfg = KubernetesConfig::for_testing();
        let env = env_of(&job(&cfg));

        assert_eq!(
            SCIP_JOB_DEADLINE_ENV, "DJINN_WARM_JOB_DEADLINE_SECONDS",
            "this is the literal string `djinn_graph::scip_indexer::budget` \
             looks up; changing it here without changing it there silently \
             unbounds the budget"
        );
        assert_eq!(
            env.get(SCIP_JOB_DEADLINE_ENV).map(String::as_str),
            Some(cfg.scip_job_timeout_seconds.to_string().as_str()),
            "the projected deadline must be THIS Job's activeDeadlineSeconds"
        );
        assert_eq!(
            job(&cfg)
                .spec
                .as_ref()
                .and_then(|s| s.active_deadline_seconds),
            Some(cfg.scip_job_timeout_seconds),
            "…and it must equal the deadline the kubelet actually enforces"
        );
        assert!(
            !env.contains_key("DJINN_SCIP_JOB_DEADLINE_SECONDS"),
            "the SCIP-specific key is read by nothing; rendering it back would \
             restore the unbounded budget this test exists to prevent"
        );
    }

    #[test]
    fn scheduling_hints_propagate_from_config() {
        let mut cfg = KubernetesConfig::for_testing();
        cfg.node_selector
            .insert("workload-type".into(), "djinn".into());
        cfg.tolerations.push(Toleration {
            key: Some("workload-type".into()),
            operator: Some("Equal".into()),
            effect: Some("NoSchedule".into()),
            ..Toleration::default()
        });
        let job = job(&cfg);
        let pod = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("pod");
        assert_eq!(
            pod.node_selector
                .as_ref()
                .and_then(|n| n.get("workload-type"))
                .map(String::as_str),
            Some("djinn")
        );
        assert_eq!(pod.tolerations.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn job_name_is_deterministic_and_label_safe() {
        let a = scip_index_job_name("Proj_ABC/xyz", "AbC123def4567890");
        assert_eq!(a, scip_index_job_name("Proj_ABC/xyz", "AbC123def4567890"));
        assert_eq!(a, "djinn-scip-proj-abc-xyz-abc123def456");
        assert!(crate::label_value::is_valid_label_value(&a), "{a}");
        assert_ne!(a, scip_index_job_name("Proj_ABC/xyz", "ffffffffffffffff"));
    }
}
