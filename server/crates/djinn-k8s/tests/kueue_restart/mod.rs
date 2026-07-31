#![allow(clippy::print_stderr)]
// Shared live-cluster machinery for `tests/kueue_restart_conformance.rs`
// (`fbiy-B2b` / `c6ej` AC4 + AC5).
//
// `#![allow(dead_code)]` is the standard `tests/<name>/mod.rs` idiom. It is not
// lazy here: `tests/kueue_disruption/mod.rs` is compiled into this binary too,
// and everything below is reachable from exactly one test target.
#![allow(dead_code)]
//! Split out of `kueue_restart_conformance.rs` because the pair exceeded
//! `scripts/check-file-size.sh` (MAX_LINES 1500 / MAX_BYTES 51200) and the
//! `djinn:allow-oversize` marker is for source that genuinely cannot be
//! divided — this could be. The `#[test]` functions stay in the sibling file;
//! everything that only SERVES them lives here.
//!
//! The cluster-isolation rules, the gating split and the two `fbiy-B2a` live
//! findings this machinery is built on are documented in that sibling.

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::process::Command;
use std::sync::Arc;

use djinn_cgroup_launcher::LauncherAuthorityProtocol;
use djinn_core::events::EventBus;
use djinn_core::models::TaskRunTrigger;
use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseState, Database,
    GrantNextBuildLeaseResult, ImageRepository, InvocationLeaseAuthorityRepository,
    InvocationLeaseMode, ProjectRepository, QueueBuildLeaseInput,
};
use djinn_k8s::KubernetesConfig;
use djinn_k8s::runtime::KubernetesRuntime;
use djinn_runtime::{ResolvedCredentials, RunHandle, SessionRuntime, SupervisorFlow, TaskRunSpec};
use djinn_supervisor::ConnectionRegistry;
use djinn_supervisor::services::{InvocationLiftDecision, evaluate_invocation_lift};
use serde_json::Value;

use crate::kueue_disruption::{
    AWAIT_TICKS, CLUSTER_QUEUE, NAMESPACE, TICK, clear_task_run_jobs, delete_job,
    install_crypto_provider, kube_client, kubectl_json, kubectl_raw, live_tests_enabled, pods_of,
    rendered_task_run_job, repair_cluster_queue_for_admission, set_stop_policy, stderr, stdout,
    workload_summary,
};

// ---------------------------------------------------------------------------
// The one cluster this file may ever touch.
// ---------------------------------------------------------------------------

/// DELIBERATELY distinct from the script's default (`djinn-kueue-harness`) AND
/// from `fbiy-B2a`'s (`djinn-kueue-b2`). `down` deletes the cluster it is given,
/// so a shared name is a shared deletion between two concurrent agents.
pub const RESTART_CLUSTER: &str = "djinn-kueue-b2b";
/// kind names its context `kind-<cluster>`. Derived rather than read from the
/// kubeconfig's CURRENT context, because every context in a Djinn developer's
/// kubeconfig is a live EKS cluster and these tests DELETE objects.
pub const RESTART_CONTEXT: &str = "kind-djinn-kueue-b2b";
pub const RESTART_REGISTRY: &str = "djinn-kueue-b2b-registry";
pub const RESTART_REGISTRY_PORT: &str = "5055";

/// The repository the stub worker image is pushed to, inside this harness's own
/// registry. `setup-kueue-cluster.sh` wires the node's containerd to resolve
/// `localhost:<port>/...` through `http://<registry>:5000`, so a digest-pinned
/// ref of this repository is pullable from inside the cluster.
pub const STUB_IMAGE_REPO: &str = "djinn-b2b-worker-stub";

/// The exit code `scripts/kind/setup-kueue-cluster.sh` reserves for a refused
/// context or a reserved name.
pub const EXIT_REFUSED_TARGET: i32 = 3;

pub const PROJECT_ID: &str = "fbiy-b2b-project";
pub const IMAGE_ID: &str = "fbiy-b2b-image";

// ===========================================================================
// The assertions AC4 names
// ===========================================================================

/// Exactly one Running Pod, exactly one Workload referencing that Job, and a
/// ClusterQueue whose used quota equals that single Workload's request.
///
/// All three are read from the LIVE API server, and both censuses are taken
/// namespace-wide as well as run-scoped, on purpose. A duplicate Job under the
/// SAME task-run id shows up in the run-scoped Pod count; a duplicate Job under
/// a DIFFERENT id (a coordinator that came back without its durable id) does
/// not, and would leave "exactly one Workload owned by this Job" true while a
/// second Job sat next to it holding a second admission slot. Both are leaks a
/// restart can cause, so both are counted.
///
/// The quota comparison is the one nothing can satisfy by accident: an equality
/// against a number derived from the surviving Workload's own pod sets, not a
/// constant.
pub fn assert_converged_to_exactly_one(context: &str, task_run_id: &str, job_name: &str) {
    let running: Vec<String> = pods_of(context, task_run_id)
        .into_iter()
        .filter(|(_, _, phase)| phase == "Running")
        .map(|(name, uid, _)| format!("{name}({uid})"))
        .collect();
    assert_eq!(
        running.len(),
        1,
        "exactly one Running Pod must carry task-run {task_run_id} after the restart; saw {running:?}",
    );
    let namespace_running = running_task_run_worker_pods(context);
    assert_eq!(
        namespace_running.len(),
        1,
        "exactly one task-run worker Pod may be Running in the namespace after the restart — the \
         suite clears the namespace before every interleaving, so a second one is this restart's \
         own duplicate. saw {namespace_running:?}",
    );

    let workloads = namespace_workloads(context);
    assert_eq!(
        workloads.len(),
        1,
        "exactly one Workload must exist after the restart; a second one is a leaked admission \
         slot. workloads: {}",
        workload_summary(context),
    );
    let owners: Vec<String> = workloads[0]["metadata"]["ownerReferences"]
        .as_array()
        .map(|owners| {
            owners
                .iter()
                .filter(|owner| owner["kind"] == "Job")
                .filter_map(|owner| owner["name"].as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        owners,
        vec![job_name.to_owned()],
        "the surviving Workload must reference the adopted Job {job_name}; it references {owners:?}",
    );

    let requested = workload_total_request(&workloads[0]);
    let used = cluster_queue_usage(context);
    assert_eq!(
        used, requested,
        "ClusterQueue used quota must equal the single surviving Workload's request. This is \
         where a duplicate admission shows up as a number rather than as an object. used={used:?} \
         requested={requested:?}",
    );
    eprintln!("CONVERGED: pods={running:?} workload_request={requested:?} queue_used={used:?}");
}

/// Whether the named Job's Workload carries `Admitted=True`.
pub fn workload_is_admitted(context: &str, job_name: &str) -> bool {
    namespace_workloads(context)
        .iter()
        .filter(|workload| workload_owner_is(workload, job_name))
        .any(|workload| {
            workload["status"]["conditions"]
                .as_array()
                .is_some_and(|conditions| {
                    conditions.iter().any(|condition| {
                        condition["type"] == "Admitted" && condition["status"] == "True"
                    })
                })
        })
}

pub fn workload_owner_is(workload: &Value, job_name: &str) -> bool {
    workload["metadata"]["ownerReferences"]
        .as_array()
        .is_some_and(|owners| {
            owners
                .iter()
                .any(|owner| owner["kind"] == "Job" && owner["name"] == job_name)
        })
}

/// Block until the namespace holds no task-run worker Pod and no Workload at
/// all.
///
/// `kubectl delete job` reaps its Pods in the BACKGROUND, and Kueue retires the
/// Workload only once the Job object is gone, so a suite that started the next
/// interleaving as soon as `delete job` returned would take its namespace-wide
/// censuses over the previous test's wreckage — measured 2026-07-31, where
/// exactly that turned a passing interleaving into `saw 6 pods`. Every
/// namespace-wide "exactly one" below is only a statement about THIS restart
/// because this ran first.
pub fn await_empty_namespace(context: &str) {
    let _ = Command::new("kubectl")
        .args([
            "--context",
            context,
            "-n",
            NAMESPACE,
            "delete",
            "pods",
            "-l",
            "djinn.app/component=task-run-worker",
            "--ignore-not-found",
            "--wait=true",
            "--timeout=90s",
        ])
        .output();
    for _ in 0..AWAIT_TICKS {
        if running_task_run_worker_pods(context).is_empty()
            && namespace_workloads(context).is_empty()
        {
            return;
        }
        std::thread::sleep(TICK);
    }
    panic!(
        "the namespace never emptied before this interleaving started; pods: {:?}; workloads: {}",
        running_task_run_worker_pods(context),
        workload_summary(context),
    );
}

/// Every Running task-run worker Pod in the namespace, whatever run it belongs
/// to. The same component label the chart's own selectors use.
pub fn running_task_run_worker_pods(context: &str) -> Vec<String> {
    kubectl_json(
        context,
        &[
            "-n",
            NAMESPACE,
            "get",
            "pods",
            "-l",
            "djinn.app/component=task-run-worker",
        ],
    )["items"]
        .as_array()
        .expect("a List has items")
        .iter()
        .filter(|pod| pod["status"]["phase"].as_str() == Some("Running"))
        .filter_map(|pod| pod["metadata"]["name"].as_str().map(ToOwned::to_owned))
        .collect()
}

pub fn namespace_workloads(context: &str) -> Vec<Value> {
    kubectl_json(
        context,
        &["-n", NAMESPACE, "get", "workloads.kueue.x-k8s.io"],
    )["items"]
        .as_array()
        .expect("a List has items")
        .clone()
}

/// The `pods` / `cpu` / `memory` a Workload asks the ClusterQueue for, summed
/// over its pod sets exactly the way Kueue does: per-Pod container requests
/// multiplied by the pod-set count.
///
/// Derived from the live object rather than from the renderer's constants, so
/// it stays an independent number to compare the queue's usage against.
pub fn workload_total_request(workload: &Value) -> BTreeMap<String, i64> {
    let mut total: BTreeMap<String, i64> = BTreeMap::new();
    for pod_set in workload["spec"]["podSets"]
        .as_array()
        .expect("a Workload declares pod sets")
    {
        let count = pod_set["count"].as_i64().unwrap_or(0);
        *total.entry("pods".into()).or_default() += count;
        for container in pod_set["template"]["spec"]["containers"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(requests) = container["resources"]["requests"].as_object() else {
                continue;
            };
            for (resource, quantity) in requests {
                let raw = quantity.as_str().unwrap_or("0");
                *total.entry(resource.clone()).or_default() +=
                    count * normalized_quantity(resource, raw);
            }
        }
    }
    total
}

/// The ClusterQueue's currently reserved quota, summed across flavors.
pub fn cluster_queue_usage(context: &str) -> BTreeMap<String, i64> {
    let queue = kubectl_json(
        context,
        &["get", "clusterqueues.kueue.x-k8s.io", CLUSTER_QUEUE],
    );
    let mut total: BTreeMap<String, i64> = BTreeMap::new();
    for flavor in queue["status"]["flavorsUsage"]
        .as_array()
        .into_iter()
        .flatten()
    {
        for resource in flavor["resources"].as_array().into_iter().flatten() {
            let Some(name) = resource["name"].as_str() else {
                continue;
            };
            let raw = resource["total"].as_str().unwrap_or("0");
            *total.entry(name.to_owned()).or_default() += normalized_quantity(name, raw);
        }
    }
    // A resource the queue reports at zero and the Workload never requests must
    // not make the two maps unequal for a reason nobody cares about.
    total.retain(|_, value| *value != 0);
    total
}

/// `resource.Quantity` → a comparable integer, in the unit that resource is
/// naturally counted in (milli-CPU, bytes, whole pods).
///
/// Both sides of the equality go through this, because the API server
/// round-trips the SAME quantity in different spellings depending on where it
/// is written: a Workload keeps `100m`, while `flavorsUsage` may report `0.1`.
pub fn normalized_quantity(resource: &str, raw: &str) -> i64 {
    let raw = raw.trim();
    match resource {
        "cpu" => match raw.strip_suffix('m') {
            Some(milli) => milli
                .parse()
                .unwrap_or_else(|_| panic!("cpu quantity {raw}")),
            None => {
                let cores: f64 = raw.parse().unwrap_or_else(|_| panic!("cpu quantity {raw}"));
                (cores * 1000.0).round() as i64
            }
        },
        "memory" | "ephemeral-storage" => {
            for (suffix, scale) in [
                ("Ki", 1024_i64),
                ("Mi", 1024 * 1024),
                ("Gi", 1024 * 1024 * 1024),
                ("Ti", 1024_i64.pow(4)),
                ("k", 1000),
                ("M", 1_000_000),
                ("G", 1_000_000_000),
            ] {
                if let Some(value) = raw.strip_suffix(suffix) {
                    return value
                        .parse::<i64>()
                        .unwrap_or_else(|_| panic!("memory quantity {raw}"))
                        * scale;
                }
            }
            raw.parse()
                .unwrap_or_else(|_| panic!("memory quantity {raw}"))
        }
        _ => raw
            .parse()
            .unwrap_or_else(|_| panic!("{resource} quantity {raw}")),
    }
}

// ===========================================================================
// The durable governor — c6ej AC5, shared by the hermetic and the live half
// ===========================================================================

pub fn invocation_key(pod_uid: &str) -> BuildLeaseKey {
    BuildLeaseKey {
        consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
        consumer_id: format!("invocation-{pod_uid}"),
    }
}

/// What one run of [`drive_invocation_governor`] measured.
pub struct InvocationGovernorOutcome {
    pub cap: i64,
    pub queued: usize,
    pub authorized: i64,
    pub fence_refused_the_other_uid: bool,
    pub binding_after_refusal: Option<String>,
    pub bound_uid: String,
}

impl InvocationGovernorOutcome {
    /// The four assertions `c6ej` AC5 names, each broken by a different
    /// deletion. See the doc on
    /// [`guard_the_invocation_governor_bounds_the_cap_and_fences_the_pod_uid`].
    pub fn assert_bounded_and_fenced(&self) {
        assert!(
            self.cap < i64::try_from(self.queued).expect("two leases fit an i64"),
            "the cap {} must be strictly below the {} queued leases, or it bounds nothing",
            self.cap,
            self.queued,
        );
        assert!(
            self.authorized <= self.cap,
            "{} invocations hold a lifted cpu.max, above the cap {}",
            self.authorized,
            self.cap,
        );
        assert_eq!(
            self.authorized, self.cap,
            "the cap must be REACHED as well as respected: {} of a possible {} were authorized, \
             which is what a deleted invocation queue looks like",
            self.authorized, self.cap,
        );
        assert!(
            self.fence_refused_the_other_uid,
            "a lift presenting a DIFFERENT Pod UID against a bound lease must be refused by the \
             bound_pod_uid fence",
        );
        assert_eq!(
            self.binding_after_refusal.as_deref(),
            Some(self.bound_uid.as_str()),
            "no rejected lift may have moved the durable Pod binding",
        );
    }
}

/// Queue two invocation leases against a cap of one, grant concurrently, bind
/// the winner to its Pod UID, then present the other UID against that same
/// lease.
///
/// Driven against a real PostgreSQL database rather than a mock repository
/// because the refusal is a locked read-modify-write plus a trigger-enforced
/// immutable column: a mock proves only that the Rust `if` was written.
pub async fn drive_invocation_governor(
    db: &Database,
    first_uid: &str,
    second_uid: &str,
) -> InvocationGovernorOutcome {
    let authority = InvocationLeaseAuthorityRepository::new(db.clone());
    let seeded = authority.seed_baseline().await.expect("seed the authority");
    authority
        .set_mode_and_cap(seeded.epoch, InvocationLeaseMode::Enforce, Some(1))
        .await
        .expect("arm the invocation-lease authority through the real operator API");

    // READ BACK. Everything below uses this value, never the one written.
    let live = authority
        .read()
        .await
        .expect("read the durable authority")
        .expect("the authority row exists once seeded");
    assert_eq!(
        evaluate_invocation_lift(Ok(Some(live.clone()))),
        InvocationLiftDecision::Lift,
        "the live authority must actually authorize lifts, or the cap bounds a population that \
         never lifts and AC5 measures nothing",
    );
    let cap = live
        .cap
        .expect("an armed authority carries a reference cap");

    let repository = Arc::new(BuildLeaseRepository::new(db.clone()));
    let uids = [first_uid.to_owned(), second_uid.to_owned()];
    for uid in &uids {
        repository
            .queue(&QueueBuildLeaseInput {
                key: invocation_key(uid),
                immutable_identity: format!("pod:{uid}"),
                queue_deadline: None,
                launch_deadline: None,
                weight: 1,
            })
            .await
            .unwrap_or_else(|e| panic!("queue an invocation lease for {uid}: {e:?}"));
    }

    // Every grant the FIFO will make at the cap, taken concurrently so the
    // bound is a property of the locked transaction rather than of the order
    // this test happened to call in.
    let mut attempts = tokio::task::JoinSet::new();
    for _ in 0..uids.len() {
        let repository = repository.clone();
        attempts.spawn(async move {
            repository
                .grant_next(1, "2026-07-31T00:00:00.000Z", None)
                .await
                .expect("grant_next must not error")
        });
    }
    let mut granted = Vec::new();
    while let Some(result) = attempts.join_next().await {
        if let GrantNextBuildLeaseResult::Granted(row) = result.expect("grant task panicked") {
            granted.push(row);
        }
    }

    let mut authorized = 0i64;
    let mut bound_uid = String::new();
    let mut bound_token = None;
    for row in &granted {
        let uid = row
            .immutable_identity
            .strip_prefix("pod:")
            .expect("the identity carries its Pod UID")
            .to_owned();
        let token = row.fencing_token.expect("a granted lease carries a token");
        let bound = repository
            .bind(&invocation_key(&uid), token, &uid, None)
            .await
            .unwrap_or_else(|e| panic!("bind the lease for {uid}: {e:?}"));
        assert_eq!(bound.state, BuildLeaseState::Bound);
        assert_eq!(bound.bound_pod_uid.as_deref(), Some(uid.as_str()));
        authorized += 1;
        bound_uid = uid;
        bound_token = Some(token);
    }

    // The fence: the SAME lease, presented with the OTHER live Pod's UID.
    let (fence_refused_the_other_uid, binding_after_refusal) = match bound_token {
        Some(token) => {
            let other = uids
                .iter()
                .find(|uid| *uid != &bound_uid)
                .expect("two distinct UIDs");
            let key = invocation_key(&bound_uid);
            let refused = repository
                .bind(&key, token, other, None)
                .await
                .err()
                .map(|error| format!("{error:?}"))
                .is_some_and(|error| error.contains("pod UID does not match build lease"));
            let after = repository
                .get(&key)
                .await
                .expect("re-read the lease row")
                .expect("the lease row exists");
            (refused, after.bound_pod_uid)
        }
        None => (false, None),
    };

    InvocationGovernorOutcome {
        cap,
        queued: uids.len(),
        authorized,
        fence_refused_the_other_uid,
        binding_after_refusal,
        bound_uid,
    }
}

// ===========================================================================
// The live world: a coordinator that can be destroyed and rebuilt
// ===========================================================================

/// Everything a restart interleaving needs, and the ability to lose all of the
/// in-process half of it.
pub struct LiveWorld {
    pub context: String,
    pub db: Database,
    pub config: KubernetesConfig,
    pub tokio: tokio::runtime::Runtime,
    pub task_run_id: String,
    pub job_name: String,
    /// Every Job this world created, so `cleanup` can delete them all even
    /// after a deliberately duplicated dispatch.
    pub created_jobs: Vec<String>,
}

impl LiveWorld {
    /// `None` when the live half is disabled; callers `return` early.
    pub fn open() -> Option<Self> {
        if !live_tests_enabled() {
            return None;
        }
        install_crypto_provider();
        let context = restart_context();
        set_stop_policy(&context, "None");
        clear_task_run_jobs(&context);
        await_empty_namespace(&context);
        let repair = repair_cluster_queue_for_admission(&context);
        eprintln!("chart repair needed: {}", repair.needed());

        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("build a tokio runtime for the live coordinator");

        let (image_tag, image_digest) = ensure_worker_stub_image(&context);
        // `open_in_memory` builds a `sqlx` pool, whose background reaper needs a
        // reactor: constructing it outside a runtime panics with "this
        // functionality requires a Tokio context", measured 2026-07-31.
        let db = tokio.block_on(async {
            let db = Database::open_in_memory().expect("real Postgres test database");
            seed_dispatchable_project(&db, &image_tag, &image_digest).await;
            db
        });

        let config = restart_live_config(&context);
        let task_run_id = uuid::Uuid::now_v7().to_string();
        let job_name = format!("djinn-taskrun-{task_run_id}");
        Some(Self {
            context,
            db,
            config,
            tokio,
            task_run_id,
            job_name,
            created_jobs: Vec::new(),
        })
    }

    /// One coordinator lifetime: build a runtime from a FRESH client and
    /// registry, dispatch the durable spec through the real `prepare`, and hand
    /// back the handle.
    pub fn dispatch(&mut self) -> RunHandle {
        let context = self.context.clone();
        let config = self.config.clone();
        let db = self.db.clone();
        let spec = self.spec();
        let handle = self.tokio.block_on(async move {
            let runtime = KubernetesRuntime::from_client_with_db(
                kube_client(&context).await,
                config,
                Arc::new(ConnectionRegistry::new()),
                db,
            );
            runtime
                .prepare(&spec, &ResolvedCredentials::default())
                .await
                .expect("the coordinator dispatches the task-run")
            // `runtime` is dropped HERE — the coordinator process ends with it.
        });
        if !self.created_jobs.contains(&self.job_name) {
            self.created_jobs.push(self.job_name.clone());
        }
        handle
    }

    /// The kill. Everything the dead coordinator knew that was not durable goes
    /// with it — most importantly the handle, which is what a task-state write
    /// would have persisted.
    pub fn restart(&self, handle: RunHandle) {
        eprintln!(
            "COORDINATOR KILLED: dropping the RunHandle for task-run {} (pod_ref={:?}) without \
             persisting anything",
            self.task_run_id, handle.pod_ref,
        );
        drop(handle);
    }

    /// The mutation: a coordinator that came back WITHOUT its durable task-run
    /// id, so nothing it dispatches can collide with what it already created.
    pub fn remint_task_run_id(&mut self) {
        self.task_run_id = uuid::Uuid::now_v7().to_string();
        self.job_name = format!("djinn-taskrun-{}", self.task_run_id);
        eprintln!(
            "MUTATION: the restarted coordinator lost its durable id and will dispatch {} instead",
            self.job_name,
        );
    }

    pub fn spec(&self) -> TaskRunSpec {
        TaskRunSpec {
            task_run_id: self.task_run_id.clone(),
            task_attempt_id: None,
            task_id: "fbiy-b2b-task".into(),
            project_id: PROJECT_ID.into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "task/fbiy-b2b".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: Vec::new(),
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        }
    }

    /// Poll until this run has a `Running` Pod, returning `(name, uid)`.
    pub fn await_running_pod(&self) -> (String, String) {
        for _ in 0..AWAIT_TICKS {
            if let Some((name, uid, _)) = pods_of(&self.context, &self.task_run_id)
                .into_iter()
                .find(|(_, uid, phase)| !uid.is_empty() && phase == "Running")
            {
                return (name, uid);
            }
            std::thread::sleep(TICK);
        }
        panic!(
            "no Running Pod appeared for task-run {}; workloads: {}; pods: {:?}",
            self.task_run_id,
            workload_summary(&self.context),
            pods_of(&self.context, &self.task_run_id),
        );
    }

    /// Poll until Kueue has CREATED the Workload for this Job, returning its
    /// name. Distinct from waiting for admission: interleaving (a) needs the
    /// object to exist so it can assert the object is not admitted.
    pub fn await_workload_for_job(&self) -> String {
        for _ in 0..AWAIT_TICKS {
            if let Some(name) = namespace_workloads(&self.context)
                .iter()
                .find(|workload| workload_owner_is(workload, &self.job_name))
                .and_then(|workload| workload["metadata"]["name"].as_str())
            {
                return name.to_owned();
            }
            std::thread::sleep(TICK);
        }
        panic!(
            "Kueue never created a Workload for {}; workloads: {}",
            self.job_name,
            workload_summary(&self.context),
        );
    }

    pub fn cleanup(&self) {
        for job in &self.created_jobs {
            delete_job(&self.context, job);
        }
        set_stop_policy(&self.context, "None");
    }
}

/// The context every live call is pinned to, after TWO independent refusals of
/// anything else.
///
/// Guard 1 is the name. Guard 2 is the resolved API-server URL, which catches
/// what guard 1 cannot: a kubeconfig entry NAMED `kind-djinn-kueue-b2b` that
/// points somewhere else. kind always serves on loopback; no managed control
/// plane does, and every context in a Djinn developer's kubeconfig is EKS.
pub fn restart_context() -> String {
    let requested =
        env::var("DJINN_TEST_KUEUE_B2B_CONTEXT").unwrap_or_else(|_| RESTART_CONTEXT.into());
    assert_eq!(
        requested, RESTART_CONTEXT,
        "this harness only ever targets the context of the cluster it creates and deletes",
    );
    let server = kubectl_raw(
        &requested,
        &[
            "config",
            "view",
            "--minify",
            "-o",
            "jsonpath={.clusters[0].cluster.server}",
        ],
    );
    assert!(
        server.starts_with("https://127.0.0.1:")
            || server.starts_with("https://localhost:")
            || server.starts_with("https://[::1]:"),
        "refusing to run against {server}: context {requested} does not resolve to a local kind \
         API server, so it is not a cluster this harness created",
    );
    requested
}

/// The armed `KubernetesConfig` this file renders with.
///
/// `cgroup_launcher_mode: Disabled` mirrors the values fixture: the renderer
/// PANICS if a required launcher is rendered without the RuntimeClass, and this
/// cluster deliberately has none (`fbiy-C1` owns installing it). The requests
/// are lowered from the production defaults so a single kind node can hold the
/// Pod at all.
pub fn restart_harness_config(image: &str) -> KubernetesConfig {
    KubernetesConfig {
        namespace: NAMESPACE.into(),
        kueue_armed: true,
        kueue_local_queue_prefix: "djinn".into(),
        cgroup_launcher_mode: djinn_k8s::launcher::CgroupLauncherMode::Disabled,
        task_run_cgroup_writable_enabled: false,
        image: image.into(),
        image_pull_policy: "IfNotPresent".into(),
        cpu_request: "100m".into(),
        cpu_limit: "500m".into(),
        memory_request: "64Mi".into(),
        memory_limit: "256Mi".into(),
        ..KubernetesConfig::for_testing()
    }
}

/// The same config, with the ServiceAccount and PVC names the CHART actually
/// created read off the live cluster.
///
/// `KubernetesConfig::for_testing()` carries the unprefixed development names
/// while the chart renders `djinn.fullname` ones. A Pod referencing a
/// nonexistent ServiceAccount or PVC never leaves `Pending`, and every
/// convergence measurement here would then be a measurement of that mistake.
pub fn restart_live_config(context: &str) -> KubernetesConfig {
    let named = |kind: &str, suffix: &str| -> String {
        kubectl_json(context, &["-n", NAMESPACE, "get", kind])["items"]
            .as_array()
            .expect("a List has items")
            .iter()
            .filter_map(|item| item["metadata"]["name"].as_str())
            .find(|name| name.ends_with(suffix))
            .unwrap_or_else(|| panic!("the chart installs a {kind} ending in {suffix}"))
            .to_owned()
    };
    KubernetesConfig {
        service_account: named("serviceaccounts", "-taskrun"),
        mirror_pvc: named("persistentvolumeclaims", "-mirrors"),
        cache_pvc: named("persistentvolumeclaims", "-cache"),
        ..restart_harness_config("replaced-by-the-catalog-image")
    }
}

/// The absolute path the rendered task-run container actually executes.
pub fn rendered_worker_command_path(config: &KubernetesConfig) -> String {
    let (job, _) = rendered_task_run_job(config);
    job.spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .map(|pod| &pod.containers)
        .and_then(|containers| containers.first())
        .and_then(|container| container.command.as_ref())
        .and_then(|command| command.first())
        .cloned()
        .expect("the renderer invokes the worker binary explicitly")
}

pub fn stub_dockerfile(worker_bin: &str) -> String {
    let dir = worker_bin
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("/");
    format!(
        "FROM busybox:1.36\n\
         RUN mkdir -p {dir} \\\n \
         && printf '#!/bin/sh\\nexec sleep 100000\\n' > {worker_bin} \\\n \
         && chmod 0755 {worker_bin}\n"
    )
}

/// Build and push a worker image that EXISTS, runs as uid 1000 and does
/// nothing, and return `(tag, digest)`.
///
/// The real worker binary is not in play: this file measures Job/Workload/Pod
/// identity across a restart, all of which are properties of the objects rather
/// than of what runs inside them, and a real worker would exit non-zero against
/// this cluster's absent server within seconds — turning every convergence
/// measurement into a measurement of that crash.
///
/// Pushed to this harness's registry rather than `kind load`ed because the
/// dispatch path resolves a DIGEST-pinned pull ref (`vf7a` fences images that
/// declare a launcher protocol without an immutable digest), and only a real
/// push mints a manifest digest. The digest is read back from the daemon rather
/// than parsed out of the push transcript.
pub fn ensure_worker_stub_image(context: &str) -> (String, String) {
    let worker_bin = rendered_worker_command_path(&restart_harness_config("unused"));
    let dir = env::temp_dir().join(format!("djinn-b2b-stub-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the stub build context");
    std::fs::write(dir.join("Dockerfile"), stub_dockerfile(&worker_bin))
        .expect("write the stub Dockerfile");

    let tag = format!("localhost:{RESTART_REGISTRY_PORT}/{STUB_IMAGE_REPO}:1");
    for (step, args) in [
        (
            "build",
            vec!["build", "-t", &tag, dir.to_str().expect("utf-8 temp dir")],
        ),
        ("push", vec!["push", &tag]),
    ] {
        let output = Command::new("docker")
            .args(&args)
            .output()
            .expect("docker is on PATH");
        assert!(
            output.status.success(),
            "docker {step} of the stub worker image failed: {}",
            stderr(&output),
        );
    }

    let inspected = Command::new("docker")
        .args([
            "image",
            "inspect",
            "-f",
            "{{range .RepoDigests}}{{println .}}{{end}}",
            &tag,
        ])
        .output()
        .expect("docker is on PATH");
    assert!(
        inspected.status.success(),
        "docker image inspect failed: {}",
        stderr(&inspected),
    );
    let prefix = format!("localhost:{RESTART_REGISTRY_PORT}/{STUB_IMAGE_REPO}@");
    let digest = stdout(&inspected)
        .lines()
        .find_map(|line| line.trim().strip_prefix(&prefix).map(ToOwned::to_owned))
        .unwrap_or_else(|| {
            panic!(
                "the pushed stub image has no repo digest for {prefix}; docker reported: {}",
                stdout(&inspected)
            )
        });
    assert!(
        digest.starts_with("sha256:") && digest.len() == 71,
        "the registry must mint a canonical manifest digest; got {digest}",
    );
    eprintln!("stub worker image: {tag}@{digest} (installs {worker_bin}) on {context}");
    (tag, digest)
}

/// Seed the durable rows the dispatch path reads BEFORE any Kubernetes object
/// is created: a project, and a catalog image that is ready, digest-pinned and
/// declares its launcher authority protocol.
///
/// All three are load-bearing. `resolve_dispatch_image` hard-fails a project
/// with no ready catalog image; `vf7a`'s fence refuses an image that declares a
/// protocol without an immutable digest; and `render_authority_protocol`
/// refuses an image that declares neither. This goes through the real
/// repositories so the fence is the production one.
pub async fn seed_dispatchable_project(db: &Database, image_tag: &str, image_digest: &str) {
    db.ensure_initialized()
        .await
        .expect("initialize the database");
    ProjectRepository::new(db.clone(), EventBus::noop())
        .create_with_id(PROJECT_ID, "fbiy-b2b", "djinn-test", PROJECT_ID)
        .await
        .expect("seed the dispatching project");
    let images = ImageRepository::new(db.clone());
    images
        .create(IMAGE_ID, "fbiy-b2b-stub", None, "{}")
        .await
        .expect("seed the catalog image");
    images
        .set_project_image(PROJECT_ID, Some(IMAGE_ID))
        .await
        .expect("assign the catalog image");
    images
        .mark_ready(
            IMAGE_ID,
            image_tag,
            Some(image_digest),
            Some(LauncherAuthorityProtocol::LeafV1),
        )
        .await
        .expect("mark the catalog image ready");

    let resolved = ProjectRepository::new(db.clone(), EventBus::noop())
        .resolve_dispatch_image(PROJECT_ID)
        .await
        .expect("the seeded project resolves a dispatch image")
        .expect("the seeded project has a dispatch image");
    assert_eq!(
        resolved.pull_ref().as_deref(),
        Some(
            format!("localhost:{RESTART_REGISTRY_PORT}/{STUB_IMAGE_REPO}@{image_digest}").as_str()
        ),
        "the dispatch path must resolve the digest-pinned stub, or the Pod pulls something else",
    );
}
