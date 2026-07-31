//! **The operator entry point, driven end to end** (proposal `3i92`, task
//! `eeky-2`).
//!
//! # What is real here
//!
//! | thing under test | fixture |
//! |---|---|
//! | the composition | **`ResizeRollout::production`** — the real one. Not `ResizeRollout::new` with stand-ins: every test below goes through `djinn_server::authority_cutover::run`, which is the function `bin/authority_cutover.rs`'s `main` is a shell around. |
//! | the drain fence, the catalog, the signed allowlist, the authority singleton | **real PostgreSQL**, via `Database::ephemeral()`. |
//! | admission pause | **`DurableAdmissionControl`** — the real `dispatch_pauses` state, read back through the coordinator's own refusal predicate. |
//! | retention | **a real HTTP round trip** to a real OCI-manifest endpoint on a real TCP socket, whose response body is hashed. |
//! | the preflight | **`djinn_k8s::cutover_preflight::run`**, assembled by the same `cutover_preflight_driver` module `bin/cutover-preflight.rs` uses, over a rendered-manifest document on disk. |
//! | the Pod plane | **`KubernetesTaskRunPodPlane`** over a real `kube::Client` whose transport is in-process and records every request. `list_taskrun_jobs`, the URL it builds and the response it deserializes are all production code. |
//!
//! # Why the apiserver is in-process and not a cluster
//!
//! Every `KubernetesRuntime` constructor but `from_client` resolves through
//! `kube::Client::try_default()` — the ambient kubeconfig, which on a developer
//! machine is the live production cluster. Composing the production Pod plane
//! against that would silently touch production. So the client is built by
//! `djinn_k8s::runtime_fixture`, which fakes the transport and nothing above
//! it, and the recorder it hands back is how "zero Pod creations" is asserted:
//! against the WIRE, not against a counter the code under test increments.
//!
//! # The governing question
//!
//! "What stays green if the body does nothing?" Every criterion below is
//! asserted against the durable authority row, the durable pause predicate and
//! the recorded requests — never against a returned `Err` alone. An error
//! returned *after* a flip satisfies an `Err`-only assertion and fails every
//! assertion in this file.

use std::sync::Arc;
use std::sync::{LazyLock, Mutex};

use djinn_db::launcher_compatibility::LegacyDigestInventory;
use djinn_db::{
    AcquireBuildPodPermitResult, BuildPodPermitRepository, BuildPodResizeIdentity,
    CaptureBuildPodResizeIdentityResult, Database, DispatchPauseRepository, ImageRepository,
    LauncherAuthorityModeRepository, SetLauncherAuthorityModeResult,
};
use djinn_k8s::runtime_fixture::{RecordedApiserver, empty_task_run_cluster};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use djinn_server::authority_cutover::{
    CutoverDirection, CutoverFailure, CutoverPlanDocument, CutoverRequest,
    RetainedArtifactDocument, run,
};
use djinn_server::task_run_resize_rollout::{
    AdmissionControl as _, DurableAdmissionControl, RolloutBlocked, RolloutStep,
};
use ring::signature::KeyPair as _;
use serde_json::{Value, json};

const LEAF: LauncherAuthorityProtocol = LauncherAuthorityProtocol::LeafV1;
const RESIZE: LauncherAuthorityProtocol = LauncherAuthorityProtocol::ResizeV2;

/// One task carries every fixture task run. `tasks.id` is `varchar(36)`.
const EEKY_TASK_ID: &str = "eeky2-task";

// ═══ process-wide environment ═══════════════════════════════════════════════

/// The signed legacy-digest inventory this binary runs under.
///
/// `ResizeRollout::production` resolves it through
/// `LegacyDigestInventory::process()`, a `OnceLock` over the environment, so it
/// is fixed for the life of the process and cannot be varied per test. Every
/// test here therefore runs under the SAME verified, empty inventory — which is
/// the honest configuration for a catalog whose every row declares a protocol,
/// and is what makes the allowlist step pass on its own evidence rather than
/// because nothing checked it.
///
/// Written and installed exactly once, before the first `process()` call, and
/// asserted to have produced a `Verified` inventory: an `Unconfigured` one would
/// block every cutover below at step 1b for a reason unrelated to what each
/// test is about.
struct CutoverEnvironment {
    scratch: std::path::PathBuf,
}

static ENVIRONMENT: LazyLock<CutoverEnvironment> = LazyLock::new(|| {
    let scratch = std::env::temp_dir().join(format!("eeky2-{}", uuid::Uuid::now_v7().simple()));
    std::fs::create_dir_all(&scratch).expect("scratch directory");

    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let key = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let document = serde_json::to_vec(&json!({
        "schema_version": 1,
        "issuer": "platform-ops",
        "issued_at": "2026-07-31T00:00:00Z",
        "digests": [],
    }))
    .unwrap();
    let path = scratch.join("legacy-digests.json");
    std::fs::write(&path, &document).expect("write the signed document");

    // SAFETY: this runs exactly once, inside a `LazyLock`, before anything in
    // this binary reads the inventory environment — `LegacyDigestInventory::process()`
    // is forced below, inside the same initializer, so no later test can observe
    // a half-installed environment.
    unsafe {
        std::env::set_var("DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY", &path);
        std::env::set_var(
            "DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY_PUBLIC_KEY",
            b64(key.public_key().as_ref()),
        );
        std::env::set_var(
            "DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY_SIGNATURE",
            b64(key.sign(&document).as_ref()),
        );
    }

    assert!(
        matches!(
            LegacyDigestInventory::process(),
            LegacyDigestInventory::Verified { .. }
        ),
        "the process inventory must be signature-verified, or every cutover below blocks at the \
         allowlist step for a reason no test in this file is about"
    );
    CutoverEnvironment { scratch }
});

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A path under this binary's scratch directory, unique per caller.
fn scratch_file(name: &str) -> std::path::PathBuf {
    ENVIRONMENT
        .scratch
        .join(format!("{}-{name}", uuid::Uuid::now_v7().simple()))
}

// ═══ the rendered manifest surface ══════════════════════════════════════════

/// A render that satisfies both render-derived preflight classes.
///
/// It is deliberately minimal rather than a `helm template` of the shipped
/// chart: whether the CHART satisfies the preflight is `djinn-k8s`'s
/// `tests/cutover_preflight.rs` question, asked there against a live render.
/// What these tests ask is whether the CUTOVER DRIVER refuses when the
/// preflight refuses, and for that the render has to be something a test can
/// mutate by exactly one field.
///
/// * a namespaced `Role` granting the exact `("", pods/resize, patch)` triple;
/// * a `ServiceAccount` labelled `app.kubernetes.io/component: taskrun`, so the
///   binding check has an identity to look for instead of matching nothing;
/// * no `RoleBinding` naming it.
fn clean_render() -> Vec<Value> {
    vec![
        json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "Role",
            "metadata": { "name": "djinn-controller", "namespace": "djinn" },
            "rules": [
                {
                    "apiGroups": [""],
                    "resources": ["pods", "pods/resize"],
                    "verbs": ["get", "list", "patch"],
                },
            ],
        }),
        json!({
            "apiVersion": "v1",
            "kind": "ServiceAccount",
            "metadata": {
                "name": "djinn-taskrun",
                "namespace": "djinn",
                "labels": { "app.kubernetes.io/component": "taskrun" },
            },
        }),
    ]
}

/// **THE MUTATION for the preflight criterion.** One field: the `pods/resize`
/// resource, removed from the only rule that grants it.
///
/// Everything else — the same Role, the same verbs, the same apiGroup, the same
/// ServiceAccount — is byte-identical to [`clean_render`]. Without the
/// subresource the lift is a 403 and every brokered build silently runs at the
/// unleased floor, which is exactly the class `pods-resize-rbac` exists to
/// catch.
fn render_without_the_resize_grant() -> Vec<Value> {
    let mut documents = clean_render();
    documents[0]["rules"][0]["resources"] = json!(["pods"]);
    documents
}

/// Write a render where the driver expects to find one, and return the path.
fn write_render(documents: &[Value]) -> String {
    let path = scratch_file("render.json");
    std::fs::write(&path, serde_json::to_vec(documents).unwrap()).expect("write the render");
    path.to_string_lossy().into_owned()
}

// ═══ seeding ════════════════════════════════════════════════════════════════

async fn assert_real_postgres(db: &Database) {
    db.ensure_initialized()
        .await
        .expect("the ephemeral database must materialize");
    let version: String = sqlx::query_scalar("SELECT version()")
        .fetch_one(db.pool())
        .await
        .expect("the fence, catalog and authority properties are PostgreSQL properties");
    assert!(
        version.starts_with("PostgreSQL"),
        "these tests must run against real PostgreSQL, got {version:?}"
    );
}

fn digest(seed: char) -> String {
    format!("sha256:{}", seed.to_string().repeat(64))
}

/// Seed `users -> projects -> tasks -> task_runs`, so real permit rows can
/// exist. `build_pod_permits.task_run_id` is a restricted foreign key.
async fn seed_project_and_run(db: &Database, task_run_id: &str) {
    db.ensure_initialized().await.unwrap();
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO users (id, github_id, github_login) \
         VALUES ('00000000-0000-7000-8000-0000000000e2', 9000000239, 'eeky2-cutover') \
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('eeky2-project', 'eeky2-project', 'djinnos', 'eeky') ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, labels, acceptance_criteria, \
          memory_refs, created_by_user_id) \
         VALUES ($1, 'eeky2-project', $2, 't', 'd', 'g', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, \
                 '00000000-0000-7000-8000-0000000000e2') ON CONFLICT DO NOTHING",
    )
    .bind(EEKY_TASK_ID)
    .bind("eeky2")
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) \
         VALUES ($1, 'eeky2-project', $2, 'manual', 'running') ON CONFLICT DO NOTHING",
    )
    .bind(task_run_id)
    .bind(EEKY_TASK_ID)
    .execute(pool)
    .await
    .unwrap();
}

/// Register a `ready` catalog image declaring `declared`, and select it.
///
/// The declaration is a parameter because the catalog a cutover validates is
/// the catalog for the mode it is moving TO: a forward cutover validates
/// `resize-v2` rows, and a rollback validates `leaf-v1` ones. Seeding one
/// declaration for both would make the rollback tests block on a catalog
/// mismatch and never reach the property they are about.
async fn seed_selected_image(
    db: &Database,
    id: &str,
    digest: &str,
    declared: LauncherAuthorityProtocol,
) {
    let repo = ImageRepository::new(db.clone());
    repo.create(id, id, None, "{}").await.unwrap();
    repo.mark_ready(id, &format!("reg/{id}:tag"), Some(digest), Some(declared))
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET selected_image_id = $1 WHERE id = 'eeky2-project'")
        .bind(id)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        repo.get(id).await.unwrap().is_some(),
        "the catalog row the probe dispatch resolves must exist"
    );
}

/// Drive a permit into a nonterminal resize state through production repository
/// methods only, so the row is exactly what production writes.
async fn seed_nonterminal_resize_row(db: &Database, task_run_id: &str) -> String {
    let permits = BuildPodPermitRepository::new(db.clone());
    let AcquireBuildPodPermitResult::Acquired { row, .. } = permits.acquire(task_run_id, 8).await
    else {
        panic!("the permit pool must admit the fixture run");
    };
    permits
        .bind_or_refresh_job_uid(
            task_run_id,
            &row.permit_id,
            row.fencing_token,
            "job-uid-eeky2",
        )
        .await
        .map(|_| ())
        .unwrap_or_else(|error| panic!("binding a Job UID must succeed: {error}"));
    let pod_uid = format!("uid-{task_run_id}");
    let captured = permits
        .capture_resize_identity(
            task_run_id,
            &row.permit_id,
            row.fencing_token,
            &BuildPodResizeIdentity {
                pod_namespace: "djinn".into(),
                pod_name: "taskrun-eeky2".into(),
                pod_uid: pod_uid.clone(),
                launcher_container_name: "cgroup-launcher".into(),
                launcher_container_id: "containerd://eeky2".into(),
                image_digest: digest('a'),
                observed_launcher_protocol: RESIZE.as_wire().into(),
                effective_launcher_protocol: RESIZE.as_wire().into(),
                admitted_cpu_millicores: 4000,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        captured,
        CaptureBuildPodResizeIdentityResult::Captured(_)
    ));
    pod_uid
}

/// Read the durable authority row, so "the mode did not move" is asserted
/// against PostgreSQL rather than against the driver's own memory.
async fn authority(db: &Database) -> (LauncherAuthorityProtocol, i64) {
    let row = LauncherAuthorityModeRepository::new(db.clone())
        .read()
        .await
        .unwrap()
        .expect("migration 167 seeds the singleton");
    (row.mode, row.epoch)
}

/// The production dispatch-pause predicate — the expression the coordinator's
/// dispatch loop guards on, not a read of the `dispatch_pauses` row.
async fn dispatch_is_paused(db: &Database) -> bool {
    DurableAdmissionControl::new(db.clone(), djinn_core::events::EventBus::noop(), "eeky2")
        .dispatch_is_paused()
        .await
        .expect("the pause state must be readable")
}

// ═══ the registry ═══════════════════════════════════════════════════════════

/// A real OCI-manifest endpoint on a real TCP socket. Deleting an entry here is
/// a genuine "the artifact is no longer pullable", not a flag flip.
struct LocalRegistry {
    manifests: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    base_url: String,
}

impl LocalRegistry {
    async fn start() -> Self {
        let manifests: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        let state = Arc::clone(&manifests);
        let app = axum::Router::new().route(
            "/v2/{repository}/manifests/{reference}",
            axum::routing::get(
                move |axum::extract::Path((repository, reference)): axum::extract::Path<(
                    String,
                    String,
                )>| {
                    let state = Arc::clone(&state);
                    async move {
                        let key = format!("{repository}/{reference}");
                        match state.lock().unwrap().get(&key).cloned() {
                            Some(bytes) => (axum::http::StatusCode::OK, bytes),
                            None => (axum::http::StatusCode::NOT_FOUND, Vec::new()),
                        }
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            manifests,
            base_url,
        }
    }

    fn push(&self, repository: &str, body: &[u8]) -> String {
        use sha2::{Digest as _, Sha256};
        let digest = format!("sha256:{:x}", Sha256::digest(body));
        self.manifests
            .lock()
            .unwrap()
            .insert(format!("{repository}/{digest}"), body.to_vec());
        digest
    }

    fn delete(&self, repository: &str, reference: &str) {
        self.manifests
            .lock()
            .unwrap()
            .remove(&format!("{repository}/{reference}"));
    }
}

// ═══ the request ════════════════════════════════════════════════════════════

/// Build the request the binary would build, without going through the
/// environment: `CutoverRequest::from_env` is exercised separately by
/// [`the_request_refuses_a_missing_or_malformed_direction`], and every test
/// below needs its own render path in the same process.
fn request(
    direction: CutoverDirection,
    render: &str,
    registry_base_url: &str,
    expected_epoch: i64,
    retained: Vec<RetainedArtifactDocument>,
    probe_image_id: &str,
    probe_task_run_id: &str,
) -> CutoverRequest {
    let plan_path = scratch_file("plan.json");
    std::fs::write(
        &plan_path,
        serde_json::to_vec(&json!({
            "expected_epoch": expected_epoch,
            "registry_base_url": registry_base_url,
            "reason": "eeky-2 cutover",
            "probe_task_run_id": probe_task_run_id,
            "probe_image_id": probe_image_id,
            "retained": retained
                .iter()
                .map(|artifact| json!({
                    "image_id": artifact.image_id,
                    "repository": artifact.repository,
                    "digest": artifact.digest,
                    "role": artifact.role,
                }))
                .collect::<Vec<_>>(),
        }))
        .unwrap(),
    )
    .unwrap();
    CutoverRequest {
        direction,
        rendered_manifests_path: render.to_owned(),
        plan: CutoverPlanDocument::load(&plan_path.to_string_lossy()).expect("a valid plan"),
        paused_by: "eeky2-cutover".into(),
    }
}

fn retained(
    image_id: &str,
    repository: &str,
    digest: &str,
    role: &str,
) -> RetainedArtifactDocument {
    RetainedArtifactDocument {
        image_id: image_id.into(),
        repository: repository.into(),
        digest: digest.into(),
        role: role.into(),
    }
}

/// A fresh, seeded database plus the in-process apiserver and registry.
struct Deployment {
    db: Database,
    runtime: Arc<djinn_k8s::runtime::KubernetesRuntime>,
    apiserver: RecordedApiserver,
    registry: LocalRegistry,
    current_digest: String,
}

impl Deployment {
    async fn start(task_run_id: &str, declared: LauncherAuthorityProtocol) -> Self {
        LazyLock::force(&ENVIRONMENT);
        let db = Database::ephemeral().await.unwrap();
        assert_real_postgres(&db).await;
        seed_project_and_run(&db, task_run_id).await;
        let registry = LocalRegistry::start().await;
        let current_digest = registry.push(
            "djinn-image-modern",
            br#"{"schemaVersion":2,"role":"resize-v2"}"#,
        );
        seed_selected_image(&db, "modern", &current_digest, declared).await;
        let (runtime, apiserver) = empty_task_run_cluster("djinn");
        Self {
            db,
            runtime,
            apiserver,
            registry,
            current_digest,
        }
    }

    fn retained_current(&self) -> RetainedArtifactDocument {
        retained(
            "modern",
            "djinn-image-modern",
            &self.current_digest,
            "resize-v2-current",
        )
    }

    async fn run(&self, request: &CutoverRequest) -> Result<Vec<RolloutStep>, CutoverFailure> {
        run(
            self.db.clone(),
            djinn_core::events::EventBus::noop(),
            Arc::clone(&self.runtime),
            request,
        )
        .await
        .map(|report| {
            assert_eq!(
                report.dispatches_admitted_while_paused, 0,
                "no Pod may be created between the pause and the resume"
            );
            report.journal
        })
    }
}

// ═══ AC1 — the entry point reaches ResizeRollout::production ════════════════

/// **The whole forward cutover, through the operator entry point.**
///
/// This is the baseline every "blocked" criterion below is measured against: it
/// proves the path can actually reach the flip, so a later test that observes an
/// unmoved mode is observing the refusal and not a path that never worked.
///
/// It also proves the apiserver recorder is LIVE — the drain proof and the
/// preflight both enumerate task-run Jobs — which is what makes
/// `workload_creations().is_empty()` a real assertion in the rollback criterion
/// rather than an assertion about a dead fixture.
#[tokio::test]
async fn the_operator_entry_point_runs_the_whole_forward_cutover() {
    let deployment = Deployment::start("019fc000-1111-7000-8000-000000000001", RESIZE).await;
    let render = write_render(&clean_render());
    let request = request(
        CutoverDirection::Activate,
        &render,
        &deployment.registry.base_url,
        0,
        vec![deployment.retained_current()],
        "modern",
        "019fc000-1111-7000-8000-000000000002",
    );

    let journal = deployment.run(&request).await.expect("a clean cutover");
    assert_eq!(
        journal,
        vec![
            RolloutStep::CatalogMutationFrozen,
            RolloutStep::LegacyInventorySigned,
            RolloutStep::ProtocolAwareServerDeployed,
            RolloutStep::CatalogRebuiltAsResizeV2,
            RolloutStep::RetentionVerified,
            RolloutStep::AdmissionPaused,
            RolloutStep::DrainProven,
            RolloutStep::PreflightCleared,
            RolloutStep::AuthorityModeFlipped,
            RolloutStep::AdmissionResumed,
        ],
        "the observed sequence is the assertion"
    );
    assert_eq!(authority(&deployment.db).await, (RESIZE, 1));
    assert!(
        !dispatch_is_paused(&deployment.db).await,
        "a completed cutover resumes admission"
    );

    // The apiserver was really read — twice, once for the drain proof and once
    // for the preflight's own Pod half — and nothing was created.
    let reads = deployment.apiserver.all();
    assert!(
        reads.len() >= 2 && reads.iter().all(|request| request.method == "GET"),
        "the drain proof and the preflight must both enumerate task-run Jobs, and neither may \
         write: {reads:?}"
    );
    assert!(deployment.apiserver.workload_creations().is_empty());
}

// ═══ AC2 — a failing preflight blocks the FLIP ══════════════════════════════

/// **A real preflight defect blocks the flip, and the mode does not move.**
///
/// The mutation is one field of the render — `pods/resize` removed from the one
/// Role rule that grants it — against a render that is otherwise byte-identical
/// to the one
/// [`the_operator_entry_point_runs_the_whole_forward_cutover`] flips under. So
/// the difference between "flipped" and "blocked" is that field and nothing
/// else.
///
/// # What stays green if the body does nothing
///
/// Nothing. Asserting only that an error came back would be satisfied by an
/// error returned *after* the flip, so the assertion is on the durable
/// authority row: mode `leaf-v1`, epoch `0`, unchanged. And the journal is read
/// too, because a flip that happened and was then rolled back by some later arm
/// would also leave the row looking untouched — `AuthorityModeFlipped` is
/// journaled only by a committed compare-and-swap, and it must be absent.
#[tokio::test]
async fn a_failing_preflight_blocks_the_flip_and_the_authority_row_does_not_move() {
    let deployment = Deployment::start("019fc000-2222-7000-8000-000000000001", RESIZE).await;
    let render = write_render(&render_without_the_resize_grant());
    let request = request(
        CutoverDirection::Activate,
        &render,
        &deployment.registry.base_url,
        0,
        vec![deployment.retained_current()],
        "modern",
        "019fc000-2222-7000-8000-000000000002",
    );

    let failure = deployment
        .run(&request)
        .await
        .expect_err("a render with no pods/resize grant must not flip the authority mode");
    let CutoverFailure::Blocked {
        blocked,
        journal,
        admission_left_paused,
    } = failure
    else {
        panic!("expected a blocked cutover, got {failure:?}");
    };

    let RolloutBlocked::PreflightRefused { classes, defects } = &blocked else {
        panic!("expected the preflight to be what refused, got {blocked:?}");
    };
    assert!(
        classes.iter().any(|class| class == "pods-resize-rbac"),
        "the refusal must name the class the mutation created: {classes:?}"
    );
    assert!(
        defects
            .iter()
            .any(|defect| defect.contains("pods/resize") && defect.contains("Role")),
        "the refusal must be the validator's own words, not a restatement: {defects:?}"
    );

    // THE ASSERTIONS THAT MATTER.
    assert_eq!(
        authority(&deployment.db).await,
        (LEAF, 0),
        "a blocked preflight must leave the durable authority row exactly as it was"
    );
    assert!(
        !journal.contains(&RolloutStep::PreflightCleared),
        "a blocked preflight must not journal a cleared step: {journal:?}"
    );
    assert!(
        !journal.contains(&RolloutStep::AuthorityModeFlipped),
        "the flip must never have committed: {journal:?}"
    );
    assert!(
        journal.contains(&RolloutStep::DrainProven),
        "the cutover must have got as far as the drain proof, or this test would pass for the \
         wrong reason: {journal:?}"
    );

    // Admission is left paused — read from the production predicate, not the
    // journal — because `resume_admission` is unreachable without a flip.
    assert!(admission_left_paused);
    assert!(dispatch_is_paused(&deployment.db).await);
    assert!(deployment.apiserver.workload_creations().is_empty());
}

// ═══ AC3 — a nonterminal row blocks the flip, BY NAME ═══════════════════════

/// **The refusal names the row, and `set_mode` alone could not.**
///
/// The criterion's named mutation is "route through `set_mode` directly and this
/// test must fail". That mutation is not described here, it is *executed*: the
/// second half of this test calls `LauncherAuthorityModeRepository::set_mode`
/// against the same database, in the same seeded state, and asserts that its
/// refusal — which is also correct, and also fail-closed — carries no
/// `task_run_id`, no lifecycle state and no `pod_uid`. A driver wired to
/// `set_mode` would produce that refusal and fail the assertions above.
#[tokio::test]
async fn a_nonterminal_resize_row_blocks_the_flip_and_the_refusal_names_it() {
    let task_run_id = "019fc000-3333-7000-8000-000000000001";
    let deployment = Deployment::start(task_run_id, RESIZE).await;
    let pod_uid = seed_nonterminal_resize_row(&deployment.db, task_run_id).await;
    let render = write_render(&clean_render());
    let request = request(
        CutoverDirection::Activate,
        &render,
        &deployment.registry.base_url,
        0,
        vec![deployment.retained_current()],
        "modern",
        "019fc000-3333-7000-8000-000000000002",
    );

    let failure = deployment
        .run(&request)
        .await
        .expect_err("a permit that still owes a resize decision must block the flip");
    let CutoverFailure::Blocked {
        blocked, journal, ..
    } = failure
    else {
        panic!("expected a blocked cutover, got {failure:?}");
    };

    let RolloutBlocked::NonterminalResizeRows(rows) = &blocked else {
        panic!(
            "the block must come from `list_nonterminal_resize`, which returns ROWS. \
             `set_mode`'s own fence would produce AuthorityDrainRefused with a census — that is \
             the mutation this test exists to catch. Got {blocked:?}"
        );
    };
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].task_run_id, task_run_id);
    assert_eq!(rows[0].state, "BirthConfirmed", "{rows:?}");
    assert_eq!(rows[0].pod_uid.as_deref(), Some(pod_uid.as_str()));

    // The operator-facing rendering carries all three, not a count.
    let rendered = blocked.to_string();
    for needle in [task_run_id, "BirthConfirmed", pod_uid.as_str()] {
        assert!(
            rendered.contains(needle),
            "the refusal an operator reads must contain {needle:?}: {rendered}"
        );
    }

    assert_eq!(
        authority(&deployment.db).await,
        (LEAF, 0),
        "the mode must not have moved"
    );
    assert!(!journal.contains(&RolloutStep::DrainProven));
    assert!(!journal.contains(&RolloutStep::AuthorityModeFlipped));

    // ── THE NAMED MUTATION, EXECUTED ────────────────────────────────────────
    //
    // Same database, same seeded row. `set_mode` refuses too — it is not
    // permissive — but it refuses with per-dimension COUNTS. A driver wired to
    // it would satisfy an `Err`-only assertion and fail every assertion above.
    let direct = LauncherAuthorityModeRepository::new(deployment.db.clone())
        .set_mode(0, RESIZE)
        .await;
    let SetLauncherAuthorityModeResult::DrainNotEmpty { drain, .. } = direct else {
        panic!("set_mode must refuse the same seeded state, got {direct:?}");
    };
    let census = format!("{drain:?}");
    for needle in [task_run_id, pod_uid.as_str()] {
        assert!(
            !census.contains(needle),
            "set_mode's census must NOT name {needle:?} — that is why the driver goes through \
             ResizeRollout: {census}"
        );
    }
    assert_eq!(
        authority(&deployment.db).await,
        (LEAF, 0),
        "and set_mode's own fence held too"
    );
}

// ═══ AC4 — rollback refuses, admission stays paused, no Pod starts ══════════

/// **A missing artifact blocks the rollback with zero Pod creations.**
///
/// Two runs, and the first is what makes the second mean anything:
///
/// 1. **control** — every retained digest is pullable, so the rollback runs to
///    completion and moves the mode `resize-v2` → `leaf-v1`. The apiserver
///    recorder ends up non-empty, proving the fixture observes traffic.
/// 2. **mutation** — one manifest is deleted from the registry and nothing else
///    changes. The rollback must refuse with the mode still `resize-v2`, the
///    epoch unmoved, admission still paused, and **zero** creations on the wire.
///
/// # What stays green if the body does nothing
///
/// An `Err`-only assertion passes if the mode flips first and the error arrives
/// afterwards, and it passes if a Pod was started on the way. So neither is
/// asserted by absence of an `Ok`: the mode and the epoch are read back out of
/// PostgreSQL, the pause is read through the coordinator's own predicate, and
/// the Pod count is read off the recorded requests.
#[tokio::test]
async fn a_missing_artifact_blocks_the_rollback_leaving_admission_paused_and_no_pod_started() {
    // ── 1. CONTROL ──────────────────────────────────────────────────────────
    let control = Deployment::start("019fc000-4444-7000-8000-000000000001", LEAF).await;
    arm_at_resize_v2(&control.db).await;
    let legacy = control.registry.push(
        "djinn-image-legacy",
        br#"{"schemaVersion":2,"role":"leaf-v1"}"#,
    );
    let render = write_render(&clean_render());
    let control_request = request(
        CutoverDirection::Rollback,
        &render,
        &control.registry.base_url,
        // `arm_at_resize_v2` moved the singleton to epoch 1; every flip is a
        // compare-and-swap against the epoch it was planned at.
        1,
        vec![
            control.retained_current(),
            retained("modern", "djinn-image-legacy", &legacy, "leaf-v1-rollback"),
        ],
        "modern",
        "019fc000-4444-7000-8000-000000000002",
    );
    let journal = control
        .run(&control_request)
        .await
        .expect("a rollback whose artifacts are all pullable must complete");
    assert!(journal.contains(&RolloutStep::AuthorityModeFlipped));
    assert_eq!(
        authority(&control.db).await,
        (LEAF, 2),
        "the control must really have moved the mode, or the mutation below proves nothing"
    );
    assert!(
        !control.apiserver.all().is_empty(),
        "the apiserver recorder must observe the control run, or 'zero creations' below is an \
         assertion about a dead fixture"
    );
    assert!(control.apiserver.workload_creations().is_empty());

    // ── 2. MUTATION: delete one manifest, change nothing else ───────────────
    let broken = Deployment::start("019fc000-5555-7000-8000-000000000001", LEAF).await;
    arm_at_resize_v2(&broken.db).await;
    // An operator arrives at a rollback with admission ALREADY paused, which is
    // the state a half-finished forward cutover leaves behind. The criterion is
    // that the rollback leaves it that way.
    DispatchPauseRepository::new(broken.db.clone(), djinn_core::events::EventBus::noop())
        .pause(
            djinn_db::DispatchPauseTarget::global(),
            djinn_core::models::DispatchPause {
                paused_by: "the operator who started the forward cutover".into(),
                paused_at: "2026-07-31T00:00:00Z".into(),
                reason: "forward cutover blocked".into(),
                expires_at: None,
            },
        )
        .await
        .unwrap();
    assert!(dispatch_is_paused(&broken.db).await);

    let rollback_digest = broken.registry.push(
        "djinn-image-legacy",
        br#"{"schemaVersion":2,"role":"leaf-v1"}"#,
    );
    let broken_request = request(
        CutoverDirection::Rollback,
        &render,
        &broken.registry.base_url,
        1,
        vec![
            broken.retained_current(),
            retained(
                "modern",
                "djinn-image-legacy",
                &rollback_digest,
                "leaf-v1-rollback",
            ),
        ],
        "modern",
        "019fc000-5555-7000-8000-000000000002",
    );
    broken
        .registry
        .delete("djinn-image-legacy", &rollback_digest);

    let failure = broken
        .run(&broken_request)
        .await
        .expect_err("a rollback whose leaf-v1 artifact is gone must refuse");
    let CutoverFailure::Blocked {
        blocked,
        journal,
        admission_left_paused,
    } = failure
    else {
        panic!("expected a blocked rollback, got {failure:?}");
    };
    let RolloutBlocked::RetentionUnprovable { digest, .. } = &blocked else {
        panic!("expected retention to be what refused, got {blocked:?}");
    };
    assert_eq!(digest, &rollback_digest);

    // THE ASSERTIONS THAT MATTER.
    assert_eq!(
        authority(&broken.db).await,
        (RESIZE, 1),
        "a blocked rollback must leave the mode and the epoch exactly as they were"
    );
    assert!(
        !journal.contains(&RolloutStep::AuthorityModeFlipped),
        "{journal:?}"
    );
    assert!(
        admission_left_paused && dispatch_is_paused(&broken.db).await,
        "a blocked rollback must never resume admission"
    );
    assert!(
        broken.apiserver.workload_creations().is_empty(),
        "no Pod may be started under an artifact set the rollback could not prove: {:?}",
        broken.apiserver.all()
    );
    assert!(
        broken.apiserver.mutations().is_empty(),
        "and nothing may be patched or deleted either: {:?}",
        broken.apiserver.mutations()
    );
}

/// Move a drained deployment to `resize-v2` so a rollback has somewhere to come
/// back from. Setup only — the property under test is what the DRIVER does next.
async fn arm_at_resize_v2(db: &Database) {
    let result = LauncherAuthorityModeRepository::new(db.clone())
        .set_mode(0, RESIZE)
        .await;
    assert!(
        matches!(result, SetLauncherAuthorityModeResult::Flipped { .. }),
        "the fixture must start from resize-v2, got {result:?}"
    );
}

// ═══ the request surface ════════════════════════════════════════════════════

/// The direction is parsed, never defaulted, and the plan must retain something.
#[test]
fn the_request_refuses_a_missing_or_malformed_direction() {
    assert_eq!(
        CutoverDirection::parse("activate").unwrap(),
        CutoverDirection::Activate
    );
    assert_eq!(
        CutoverDirection::parse("rollback").unwrap(),
        CutoverDirection::Rollback
    );
    assert_eq!(CutoverDirection::Activate.target(), RESIZE);
    assert_eq!(CutoverDirection::Rollback.target(), LEAF);
    for bogus in ["", "Activate", "roll-back", "resize-v2", "yes"] {
        assert!(
            CutoverDirection::parse(bogus).is_err(),
            "{bogus:?} must not be read as a direction"
        );
    }

    LazyLock::force(&ENVIRONMENT);
    let empty = scratch_file("empty-plan.json");
    std::fs::write(
        &empty,
        br#"{"expected_epoch":0,"registry_base_url":"http://r","reason":"r",
             "probe_task_run_id":"t","probe_image_id":"i","retained":[]}"#,
    )
    .unwrap();
    let error = CutoverPlanDocument::load(&empty.to_string_lossy())
        .expect_err("a plan that retains nothing proves the pullability of nothing");
    assert!(error.contains("retains nothing"), "{error}");

    let bad_role = scratch_file("bad-role-plan.json");
    std::fs::write(
        &bad_role,
        br#"{"expected_epoch":0,"registry_base_url":"http://r","reason":"r",
             "probe_task_run_id":"t","probe_image_id":"i",
             "retained":[{"image_id":"i","repository":"r","digest":"sha256:x","role":"current"}]}"#,
    )
    .unwrap();
    assert!(
        CutoverPlanDocument::load(&bad_role.to_string_lossy())
            .expect_err("an unknown retention role must be refused, not defaulted")
            .contains("role must be one of")
    );
}
