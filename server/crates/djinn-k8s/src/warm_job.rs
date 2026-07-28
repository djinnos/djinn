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
    ConfigMapVolumeSource, Container, EmptyDirVolumeSource, EnvVar, EnvVarSource,
    ObjectFieldSelector, PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec,
    ResourceRequirements, Toleration, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use uuid::Uuid;

use crate::config::KubernetesConfig;
use crate::graph_warmer_identity::LeasedWarmJobIdentity;

/// Label key identifying a graph-warm Job.
pub const LABEL_WARM: &str = "djinn.app/warm";
/// Label key for the project id a warm Job targets.
pub const LABEL_PROJECT_ID: &str = "djinn.app/project-id";
/// `djinn.app/component` value written on warm resources.
pub const COMPONENT_GRAPH_WARM: &str = "graph-warm";
/// Label key identifying which djinn-internal component created the resource.
pub const LABEL_COMPONENT: &str = "djinn.app/component";
/// Durable identity annotations used for inventory and trace correlation.
/// They are deliberately annotations rather than exported metric labels.
pub const ANNOTATION_WARM_REQUEST_ID: &str = "djinn.app/warm-request-id";
pub const ANNOTATION_GRAPH_REVISION: &str = "djinn.app/graph-revision";
pub const ANNOTATION_FENCING_TOKEN: &str = "djinn.app/fencing-token";
pub const GATE_AUTHORIZATION_KEY: &str = "authorization";
pub const VOLUME_WARM_GATE: &str = "warm-gate";

/// Durable build-lease consumer id (the lease's `warm_request_id`), projected
/// into the WARMER container so the in-Pod worker can hand the build slot back
/// the moment its cargo phase ends.
///
/// The warm Pod holds one of only three build slots for its entire life, but
/// only its cargo half is a build: measured over 6h48m on 2026-07-27 the warm
/// held a slot for 6h44m (98.9% duty cycle) and 60.7% of that hold was the SCIP
/// phase — one single-threaded rust-analyzer process averaging ~0.82 of the 4
/// cores the slot is weighted for. Releasing at the cargo→graph boundary
/// recovers ~0.6 of 3 slots with no change to graph freshness.
///
/// This is deliberately NOT a second lease authority: queue/grant/bind stay
/// host-owned. The Pod performs one fenced, idempotent release of a slot it
/// already holds.
pub const ENV_WARM_LEASE_CONSUMER_ID: &str = "DJINN_WARM_LEASE_CONSUMER_ID";
/// Fencing token for [`ENV_WARM_LEASE_CONSUMER_ID`]. A release that does not
/// carry the current token is rejected by the ledger, so a Pod outlived by a
/// newer grant can never release the new holder's slot.
pub const ENV_WARM_LEASE_FENCING_TOKEN: &str = "DJINN_WARM_LEASE_FENCING_TOKEN";

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
# Everything this Pod creates on the shared volumes must stay group-writable for
# the other identities that read/write them (djinn-server at uid 10001, the
# task-run worker at uid 1000 — both of which only manage lifecycle here and
# work through gid 1000). The container default 022 would give the clone 755
# dirs / 644 files, which the worker's startup contract check rejects. See
# `djinn_agent_worker::volume_contract`.
umask 0002
# The mirror is server-owned (uid 10001) while this pod runs as uid 1001, so git
# needs a `safe.directory` exception for it. It must be a config FILE in protected
# scope: git honours safe.directory only from system/global files in the inner
# `git-upload-pack` child of `git clone --local`, and strips command-scope config
# from that child. SYSTEM, not GLOBAL, so `git config --global` (the private-dep
# url.insteadOf token rewrite) keeps writing $HOME/.gitconfig where cargo/go/pnpm
# read it. See djinn-git/src/lib.rs and the volume-ownership runbook (nurw).
export GIT_CONFIG_SYSTEM={WORKSPACE_MOUNT_DIR}/.djinn-gitconfig
unset GIT_CONFIG_NOSYSTEM
printf '[safe]\n\tdirectory = *\n' > "$GIT_CONFIG_SYSTEM"
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
        // Project the Job's own activeDeadlineSeconds (set below from
        // `warm_job_timeout_seconds`) so the in-Pod warm can size its per-step
        // cargo budgets against the deadline that will kill it. Without this
        // the worker would bound a step by a constant that may be larger than
        // the Job deadline, which does not give the step more time — it just
        // relocates the truncation from an observable `outcome="timeout"` step
        // record to an unobservable kubelet kill. Mirrors the task-run path's
        // DJINN_TASK_RUN_ACTIVE_DEADLINE_SECONDS projection in `job.rs`.
        env_var(
            "DJINN_WARM_JOB_DEADLINE_SECONDS",
            &config.warm_job_timeout_seconds.to_string(),
        ),
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
    env.extend(crate::job::warm_cache_env_vars(
        project_id,
        &config.warm_cpu_limit,
        policy,
    ));

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
        // Why CHILD_UID (1001), not WORKER_UID (1000). The rule for this field
        // is not "match the task-run pod": it is that every actor which CREATES
        // CONTENT in the shared `/cache/cargo-target` tree must be the same uid.
        // That uid is 1001, because cargo and its build scripts always run as
        // the launcher-spawned child, and a build script copying over a seeded
        // artifact ends in `set_permissions`. `chmod`/`chown`/`utimes` are
        // governed by OWNERSHIP alone — EPERM to a non-owner even for a
        // byte-identical mode, and no mode bit, setgid, ACL or group grants
        // them. (Directory-entry and content ops DO work cross-identity through
        // gid 1000 + setgid + `g+w`; only inode metadata does not.)
        //
        // This does not touch the launcher's security boundary: that boundary is
        // the 0600 worker-owned broker socket, which refuses uid 1001 at
        // `connect(2)`, and a warm Job renders no launcher sidecar and no socket
        // at all. Asserted by `warm_pod_never_renders_a_launcher_sidecar`.
        //
        // `run_as_group`/`fsGroup` stay at ARTIFACT_GID: group remains the
        // mechanism for the lifecycle-only actors (djinn-server at 10001,
        // task-run worker at 1000). Only content creation moved.
        //
        // Moving the EXISTING base to this state is a one-time operator action:
        // `docs/CARGO_CACHE_OWNERSHIP_MIGRATION_RUNBOOK.md`.
        security_context: Some(k8s_openapi::api::core::v1::PodSecurityContext {
            run_as_user: Some(i64::from(crate::launcher::CHILD_UID)),
            run_as_group: Some(i64::from(crate::launcher::ARTIFACT_GID)),
            ..crate::launcher::pod_security_context()
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
            // Deadline margin: this ONE deadline covers both halves of the warm
            // Pod's work — `warm_cargo_target_base`'s single default-features
            // pass (clippy + build + test-compile), and then the SCIP indexing
            // + graph publication phase. A complete production warm measured on
            // 2026-07-27 spent 1798s in cargo and 3644s in the graph phase:
            // 5442s end to end. The default `warm_job_timeout_seconds` is
            // 7200s (120 min), leaving ~29 min of margin. If a larger workspace
            // consistently hits this deadline the warm Pod is SIGKILLed
            // mid-compile or mid-SCIP and the next warm tick starts over from
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

/// Build a deterministic, lease-gated Job from inputs persisted before
/// Kubernetes create. The legacy [`build_warm_job`] wrapper remains available
/// for existing callers that do not yet participate in the lease protocol.
pub fn build_leased_warm_job(
    config: &KubernetesConfig,
    project_id: &str,
    project_image_tag: &str,
    policy: Option<&djinn_stack::environment::CargoCachePolicy>,
    identity: &LeasedWarmJobIdentity,
) -> Job {
    let mut job = build_warm_job(config, project_id, project_image_tag, policy);
    job.metadata.name = Some(identity.object_name.clone());
    let annotations = BTreeMap::from([
        (
            ANNOTATION_WARM_REQUEST_ID.into(),
            identity.warm_request_id.clone(),
        ),
        (
            ANNOTATION_GRAPH_REVISION.into(),
            identity.graph_revision.clone(),
        ),
        (
            ANNOTATION_FENCING_TOKEN.into(),
            identity.fencing_token.to_string(),
        ),
    ]);
    job.metadata.annotations = Some(annotations.clone());

    let spec = job.spec.as_mut().expect("warm job always has a spec");
    spec.template.metadata.get_or_insert_default().annotations = Some(annotations);
    let pod = spec
        .template
        .spec
        .as_mut()
        .expect("warm job always has a pod spec");

    // Hand the warmer container the identity it needs to release its own build
    // slot at the cargo→graph boundary. Only the LEASED path projects these:
    // an unleased warm holds no slot and must not attempt a release.
    for container in pod.containers.iter_mut() {
        let env = container.env.get_or_insert_default();
        env.push(env_var(
            ENV_WARM_LEASE_CONSUMER_ID,
            &identity.warm_request_id,
        ));
        env.push(env_var(
            ENV_WARM_LEASE_FENCING_TOKEN,
            &identity.fencing_token.to_string(),
        ));
    }

    pod.volumes.get_or_insert_default().push(Volume {
        name: VOLUME_WARM_GATE.into(),
        config_map: Some(ConfigMapVolumeSource {
            name: warm_gate_config_map_name(&identity.object_name),
            optional: Some(true),
            ..ConfigMapVolumeSource::default()
        }),
        ..Volume::default()
    });
    // Kubernetes runs init containers to completion before the warmer starts.
    // The authorization must match this Pod's immutable UID and fencing token.
    pod.init_containers = Some(vec![warm_gate_container(project_image_tag, identity)]);
    job
}

/// Stable ConfigMap name used by the external gate controller to deliver its
/// `pod-uid:fencing-token` authorization payload.
pub(crate) fn warm_gate_config_map_name(job_name: &str) -> String {
    format!("{}-gate", &job_name[..job_name.len().min(58)])
}

fn warm_gate_container(project_image_tag: &str, identity: &LeasedWarmJobIdentity) -> Container {
    let command = format!(
        r#"set -eu
expected="${{DJINN_BOUND_POD_UID}}:${{DJINN_WARM_FENCING_TOKEN}}"
while :; do
  if [ -f /var/run/djinn-warm-gate/{key} ] && [ "$(cat /var/run/djinn-warm-gate/{key})" = "$expected" ]; then
    exit 0
  fi
  sleep 1
done"#,
        key = GATE_AUTHORIZATION_KEY,
    );
    Container {
        name: "warm-lease-gate".into(),
        image: Some(project_image_tag.into()),
        command: Some(vec!["/bin/bash".into(), "-c".into(), command]),
        env: Some(vec![
            EnvVar {
                name: "DJINN_BOUND_POD_UID".into(),
                value_from: Some(EnvVarSource {
                    field_ref: Some(ObjectFieldSelector {
                        field_path: "metadata.uid".into(),
                        ..ObjectFieldSelector::default()
                    }),
                    ..EnvVarSource::default()
                }),
                ..EnvVar::default()
            },
            env_var(
                "DJINN_WARM_FENCING_TOKEN",
                &identity.fencing_token.to_string(),
            ),
        ]),
        volume_mounts: Some(vec![VolumeMount {
            name: VOLUME_WARM_GATE.into(),
            mount_path: "/var/run/djinn-warm-gate".into(),
            read_only: Some(true),
            ..VolumeMount::default()
        }]),
        ..Container::default()
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
#[path = "warm_job_tests.rs"]
mod tests;
