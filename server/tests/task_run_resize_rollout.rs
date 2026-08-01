//! The fenced launcher-authority cutover, end to end (task `eeky`, epic `xowm`).
//!
//! # What is real here, and what is not
//!
//! | thing under test | fixture |
//! |---|---|
//! | drain fence, catalog metadata, signed allowlist | **real PostgreSQL** — `Database::ephemeral()` clones the migrated test template. Every test asserts this before it asserts anything else, through [`assert_real_postgres`]. |
//! | admission pause | **the production implementation** — `DurableAdmissionControl`, writing the real `dispatch_pauses` state and reading it back through the coordinator's own refusal predicate. |
//! | registry pullability | **a real HTTP round trip** over a real TCP socket to a real OCI-manifest endpoint, whose response body is hashed. The bytes are served in-process; the fetch, the socket, the status codes and the SHA-256 comparison are not simulated. `deploy/preflight/tests/task-run-resize-rollout.sh` runs the same check against a disposable `registry:2` container. |
//! | the Pod plane | a recording fixture. The task's design forbids standing up a live cluster for this wave, so Pod creation, `pods/resize` PATCHes and the live-Pod census are counted rather than performed. Every assertion about them is a **count of attempts**, never a read of an intent field. |
//!
//! No repository is faked. [`ResizeRollout::new`] takes a `Database` and builds
//! `BuildPodPermitRepository`, `LauncherAuthorityModeRepository` and
//! `ImageRepository` itself — there is no constructor through which a fake
//! could be passed, which is asserted at the source level by
//! [`the_driver_exposes_no_seam_for_a_fake_repository`].
//!
//! # The governing question
//!
//! "What stays green if the body does nothing?" Every criterion below is
//! asserted on an observed effect: a refused dispatch, a count of Pod creations,
//! a count of `pods/resize` PATCHes, a named row, a hashed HTTP body, an epoch
//! that did not move. No test in this file reads a `paused` row, a `pullable`
//! column, an intended-authority field, or a `signed: true` flag — none of which
//! exist in the types under test, deliberately.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_db::launcher_compatibility::{
    AdmissionRejection, LegacyDigestInventory, PreProtocolDigest,
};
use djinn_db::repositories::image::LegacyAllowlistDefect;
use djinn_db::{
    AcquireBuildPodPermitResult, BuildPodPermitRepository, BuildPodResizeIdentity,
    CaptureBuildPodResizeIdentityResult, Database, DispatchPauseRepository, Image, ImageRepository,
    LauncherAuthorityModeRepository, LauncherProtocolAdmission,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use djinn_server::task_run_resize_rollout::{
    AdmissionControl, CatalogDefectReason, CatalogVerdict, CutoverPreflight, DispatchOutcome,
    DispatchProbe, DurableAdmissionControl, HttpRegistryProbe, LiveTaskRunPod, PreflightVerdict,
    RegistryProbe, ResizeRollout, RetainedArtifact, RetentionRole, RolloutBlocked, RolloutPlan,
    RolloutStep, TaskRunPodPlane, classify_catalog_image,
};
use ring::signature::KeyPair as _;

/// One task carries every fixture task run. `tasks.id` is `varchar(36)`, so a
/// derived id would silently truncate.
const EEKY_TASK_ID: &str = "eeky-task";

const LEAF: LauncherAuthorityProtocol = LauncherAuthorityProtocol::LeafV1;
const RESIZE: LauncherAuthorityProtocol = LauncherAuthorityProtocol::ResizeV2;

async fn legacy_leaf_database() -> Database {
    let db = Database::ephemeral().await.unwrap();
    djinn_db::test_support::seed_legacy_launcher_authority_for_test(&db).await;
    db
}

// ═══ harness ════════════════════════════════════════════════════════════════

/// Prove the "database" under these tests is really PostgreSQL.
///
/// This is the source-level gate the criterion asks for, discharged at runtime
/// instead: `Database::pool()` is a `sqlx::PgPool` by type, and this executes a
/// statement only a live PostgreSQL server answers. A repository substituted for
/// a fake would not reach this function at all — `ResizeRollout` has no seam for
/// one — but a `Database` backed by anything other than PostgreSQL would fail
/// right here.
async fn assert_real_postgres(db: &Database) {
    db.ensure_initialized()
        .await
        .expect("the ephemeral database must materialize");
    let version: String = sqlx::query_scalar("SELECT version()")
        .fetch_one(db.pool())
        .await
        .expect("the drain-fence and catalog properties are PostgreSQL properties");
    assert!(
        version.starts_with("PostgreSQL"),
        "these tests must run against real PostgreSQL, got {version:?}"
    );
}

/// A canonical immutable manifest digest: `sha256:` + 64 lowercase hex.
fn digest(seed: char) -> String {
    format!("sha256:{}", seed.to_string().repeat(64))
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A signature-verified inventory over `entries`, built the way a deployment
/// builds one: sign the document bytes, then verify them.
///
/// Deliberately routed through `from_signed_document` rather than
/// `LegacyDigestInventory::verified`, so every test in this file depends on a
/// real Ed25519 verification having succeeded.
fn signed_inventory(entries: &[String]) -> LegacyDigestInventory {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let key = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let document = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "issuer": "platform-ops",
        "issued_at": "2026-07-31T00:00:00Z",
        "digests": entries,
    }))
    .unwrap();
    let inventory = LegacyDigestInventory::from_signed_document(
        &document,
        Some(&b64(key.public_key().as_ref())),
        Some(&b64(key.sign(&document).as_ref())),
    );
    assert!(
        matches!(inventory, LegacyDigestInventory::Verified { .. }),
        "the fixture inventory must be signature-verified, got {inventory:?}"
    );
    inventory
}

/// Seed the FK chain `users -> projects -> tasks -> task_runs` so real permit
/// rows can exist, and point the project at `selected_image_id`.
///
/// `build_pod_permits.task_run_id` is a restricted foreign key, so a drain
/// dimension cannot be conjured by inserting a bare row.
async fn seed_project_and_run(db: &Database, task_run_id: &str) {
    db.ensure_initialized().await.unwrap();
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO users (id, github_id, github_login) \
         VALUES ('00000000-0000-7000-8000-0000000000ee', 9000000238, 'eeky-rollout') \
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('eeky-project', 'eeky-project', 'djinnos', 'eeky') ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, labels, acceptance_criteria, \
          memory_refs, created_by_user_id) \
         VALUES ($1, 'eeky-project', $2, 't', 'd', 'g', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, \
                 '00000000-0000-7000-8000-0000000000ee') ON CONFLICT DO NOTHING",
    )
    .bind(EEKY_TASK_ID)
    .bind("eeky")
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) \
         VALUES ($1, 'eeky-project', $2, 'manual', 'running') ON CONFLICT DO NOTHING",
    )
    .bind(task_run_id)
    .bind(EEKY_TASK_ID)
    .execute(pool)
    .await
    .unwrap();
}

/// Point `eeky-project` at a catalog image, making it dispatch-eligible.
///
/// `projects.selected_image_id` is a foreign key, so this can only run after
/// the image row exists — which is why "dispatch-eligible" is a property of the
/// relation and not of a flag.
async fn select_image(db: &Database, image_id: &str) {
    sqlx::query("UPDATE projects SET selected_image_id = $1 WHERE id = 'eeky-project'")
        .bind(image_id)
        .execute(db.pool())
        .await
        .unwrap();
}

/// Register a `ready` catalog image with the given digest and declaration.
async fn seed_image(
    db: &Database,
    id: &str,
    registry_digest: Option<&str>,
    declared: Option<LauncherAuthorityProtocol>,
) -> Image {
    let repo = ImageRepository::new(db.clone());
    repo.create(id, id, None, "{}").await.unwrap();
    repo.mark_ready(id, &format!("reg/{id}:tag"), registry_digest, declared)
        .await
        .unwrap();
    repo.get(id).await.unwrap().unwrap()
}

/// Drive a permit from `acquire` into a nonterminal resize state, using only
/// production repository methods so the row is exactly what production writes.
///
/// `capture_resize_identity` sets `state = 'birth_confirmed'`, one of migration
/// 164's six nonterminal states and therefore one
/// `build_pod_permits_resize_nonterminal_idx` selects.
async fn seed_nonterminal_resize_row(db: &Database, task_run_id: &str) {
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
            "job-uid-eeky",
        )
        .await
        .map(|_| ())
        .unwrap_or_else(|error| panic!("binding a Job UID must succeed: {error}"));
    let captured = permits
        .capture_resize_identity(
            task_run_id,
            &row.permit_id,
            row.fencing_token,
            &BuildPodResizeIdentity {
                pod_namespace: "djinn".into(),
                pod_name: "taskrun-eeky".into(),
                pod_uid: format!("uid-{task_run_id}"),
                launcher_container_name: "cgroup-launcher".into(),
                launcher_container_id: "containerd://eeky".into(),
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
    assert_eq!(
        BuildPodPermitRepository::new(db.clone())
            .list_nonterminal_resize()
            .await
            .unwrap()
            .len(),
        1,
        "the fixture must actually produce a row the nonterminal index selects"
    );
}

// ── the Pod plane fixture ───────────────────────────────────────────────────

/// Records what was *attempted* against the Pod plane.
///
/// Every assertion in this file about Pods is a count taken from here. There is
/// no "intended authority" field to read: `resize_patches` moves when and only
/// when a `pods/resize` PATCH is actually issued.
#[derive(Default)]
struct RecordingPodPlane {
    pod_creations: AtomicU64,
    resize_patches: AtomicU64,
    live: Mutex<Vec<LiveTaskRunPod>>,
}

impl RecordingPodPlane {
    fn drained() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn with_live_pod() -> Arc<Self> {
        let plane = Self::default();
        plane.live.lock().unwrap().push(LiveTaskRunPod {
            pod_name: "taskrun-still-running".into(),
            pod_uid: "uid-still-running".into(),
            task_run_id: "019fb854-0000-7000-8000-00000000dead".into(),
        });
        Arc::new(plane)
    }

    fn pod_creations(&self) -> u64 {
        self.pod_creations.load(Ordering::SeqCst)
    }

    fn resize_patches(&self) -> u64 {
        self.resize_patches.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TaskRunPodPlane for RecordingPodPlane {
    async fn create_task_run_pod(&self, _task_run_id: &str, _image_id: &str) -> Result<(), String> {
        self.pod_creations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn resize_launcher_cpu(
        &self,
        _task_run_id: &str,
        _millicores: u64,
    ) -> Result<(), String> {
        self.resize_patches.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn live_task_run_pods(&self) -> Result<Vec<LiveTaskRunPod>, String> {
        Ok(self.live.lock().unwrap().clone())
    }
}

// ── the preflight seam ──────────────────────────────────────────────────────

/// A preflight whose verdict is chosen by the test, recording every call.
///
/// The REAL preflight — `djinn_k8s::cutover_preflight::run` over a live
/// `helm template` render — is driven end to end through
/// `ResizeRollout::production` in `tests/authority_cutover.rs`. What this
/// fixture is for is the *ordering* property: that a blocked preflight leaves
/// the mode untouched and that the flip is unreachable without it. Both of
/// those are properties of the journal, not of any rule inside the preflight.
struct RecordingPreflight {
    verdict: Mutex<PreflightVerdict>,
    calls: AtomicU64,
}

impl RecordingPreflight {
    /// Clean, with every class evaluated.
    fn clear() -> Arc<Self> {
        Arc::new(Self {
            verdict: Mutex::new(PreflightVerdict::Clear {
                evaluated: vec![
                    "birth-confirmation".into(),
                    "catalog-protocol".into(),
                    "credential-boundary".into(),
                    "drain-fence".into(),
                    "launcher-cpu-ceiling".into(),
                    "pods-resize-rbac".into(),
                ],
            }),
            calls: AtomicU64::new(0),
        })
    }

    fn set(&self, verdict: PreflightVerdict) {
        *self.verdict.lock().unwrap() = verdict;
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CutoverPreflight for RecordingPreflight {
    async fn evaluate(
        &self,
        _mode: LauncherAuthorityProtocol,
        _live_task_run_pods: &[String],
    ) -> PreflightVerdict {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.verdict.lock().unwrap().clone()
    }
}

// ── the registry ────────────────────────────────────────────────────────────

/// A real OCI-manifest endpoint on a real TCP socket.
///
/// It serves whatever bytes are in its map at `/v2/<repo>/manifests/<ref>` and
/// 404s otherwise, which is exactly the shape a registry takes after a manifest
/// is deleted. The probe under test does a real HTTP GET and hashes the body it
/// gets back, so removing an entry here is a genuine "the artifact is no longer
/// pullable" and not a flag flip.
struct LocalRegistry {
    manifests: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    base_url: String,
}

impl LocalRegistry {
    async fn start() -> Self {
        let manifests: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
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

    /// Push `body` and return the digest the registry would report for it —
    /// the SHA-256 of the exact bytes, which is what the probe recomputes.
    fn push(&self, repository: &str, body: &[u8]) -> String {
        use sha2::{Digest as _, Sha256};
        let digest = format!("sha256:{:x}", Sha256::digest(body));
        self.manifests
            .lock()
            .unwrap()
            .insert(format!("{repository}/{digest}"), body.to_vec());
        digest
    }

    /// Serve different bytes under an existing reference, without touching any
    /// catalog row.
    fn substitute(&self, repository: &str, reference: &str, body: &[u8]) {
        self.manifests
            .lock()
            .unwrap()
            .insert(format!("{repository}/{reference}"), body.to_vec());
    }

    /// Delete the manifest. Nothing else changes: no catalog row, no column.
    fn delete(&self, repository: &str, reference: &str) {
        self.manifests
            .lock()
            .unwrap()
            .remove(&format!("{repository}/{reference}"));
    }

    fn probe(&self) -> Arc<dyn RegistryProbe> {
        Arc::new(HttpRegistryProbe::new(&self.base_url))
    }
}

/// A probe that answers nothing, for the arms that must not depend on a
/// registry being reachable at all.
struct UnusedRegistry;

#[async_trait]
impl RegistryProbe for UnusedRegistry {
    async fn fetch_manifest(&self, _repository: &str, _reference: &str) -> Result<Vec<u8>, String> {
        panic!("this test must not reach the registry");
    }
}

// ── admission fixtures ──────────────────────────────────────────────────────

/// **The mutation, embodied.** Writes the pause row and wires no refusal.
///
/// This is exactly the failure the criterion names: a `paused` row exists, a
/// status read would report `paused`, and dispatch is nevertheless admitted. It
/// is here so the assertion "the pause step blocks on it" is a real assertion
/// rather than a claim about a hypothetical.
struct RowOnlyAdmissionControl {
    paused: Mutex<bool>,
}

#[async_trait]
impl AdmissionControl for RowOnlyAdmissionControl {
    async fn pause(&self, _reason: &str) -> Result<(), String> {
        *self.paused.lock().unwrap() = true;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        *self.paused.lock().unwrap() = false;
        Ok(())
    }

    async fn dispatch_is_paused(&self) -> Result<bool, String> {
        // The row says paused. The refusal path was never wired, so the
        // predicate the dispatcher consults keeps answering "not paused".
        Ok(false)
    }
}

fn durable_admission(db: &Database) -> Arc<dyn AdmissionControl> {
    Arc::new(DurableAdmissionControl::new(
        db.clone(),
        EventBus::noop(),
        "eeky-cutover",
    ))
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

fn probe_for(image: &Image) -> DispatchProbe {
    DispatchProbe {
        task_run_id: "019fb854-1111-7000-8000-000000000001".into(),
        image: image.clone(),
    }
}

// ═══ AC2 — pullability and retention ════════════════════════════════════════

/// Retention is a registry round trip whose *body* is hashed, and deleting the
/// manifest turns it red while every catalog row stays exactly as it was.
#[tokio::test]
async fn retention_is_proven_by_a_registry_round_trip_not_by_a_stored_column() {
    let db = legacy_leaf_database().await;
    assert_real_postgres(&db).await;
    let registry = LocalRegistry::start().await;

    let manifest =
        br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let recorded = registry.push("djinn-image-legacy", manifest);
    seed_image(&db, "legacy", Some(&recorded), None).await;

    let rollout = ResizeRollout::new(
        db.clone(),
        signed_inventory(std::slice::from_ref(&recorded)),
        durable_admission(&db),
        RecordingPodPlane::drained(),
        registry.probe(),
        RecordingPreflight::clear(),
    );
    let retained = vec![RetainedArtifact {
        image_id: "legacy".into(),
        repository: "djinn-image-legacy".into(),
        digest: recorded.clone(),
        role: RetentionRole::LegacyNoHandshake,
    }];

    rollout
        .probe_retention(&retained)
        .await
        .expect("a manifest whose body hashes to the recorded digest is retained");

    // MUTATION — delete the manifest, touch nothing else.
    registry.delete("djinn-image-legacy", &recorded);
    let blocked = rollout.probe_retention(&retained).await.unwrap_err();
    let RolloutBlocked::RetentionUnprovable { digest, detail } = blocked else {
        panic!("a deleted manifest must block retention, got {blocked:?}");
    };
    assert_eq!(digest, recorded);
    assert!(
        detail.contains("404"),
        "the block must report what the round trip actually said, got {detail:?}"
    );

    // The catalog row is untouched: the check never consulted it, so a stored
    // `pullable = true` would have changed nothing about the outcome above.
    let row = ImageRepository::new(db.clone())
        .get("legacy")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "ready");
    assert_eq!(row.registry_digest.as_deref(), Some(recorded.as_str()));

    // SECOND MUTATION — serve *different* content under the same reference. The
    // reference resolves, the fetch succeeds, and the digest does not match.
    registry.substitute("djinn-image-legacy", &recorded, b"{\"schemaVersion\":2}");
    let blocked = rollout.probe_retention(&retained).await.unwrap_err();
    let RolloutBlocked::RetentionUnprovable { detail, .. } = blocked else {
        panic!("substituted content must block retention, got {blocked:?}");
    };
    assert!(
        detail.contains("served content whose digest is"),
        "the block must name the digest actually served, got {detail:?}"
    );
}

// ═══ AC3 — catalog metadata against the mode, through the enum ══════════════

/// Under `resize-v2` every dispatch-eligible image must declare `resize-v2`;
/// under `leaf-v1` an allowlisted no-handshake digest maps to leaf authority.
#[tokio::test]
async fn catalog_metadata_is_validated_against_the_mode_and_not_only_the_declaration() {
    let db = legacy_leaf_database().await;
    assert_real_postgres(&db).await;
    seed_project_and_run(&db, "019fb854-2222-7000-8000-000000000001").await;
    seed_image(&db, "legacy", Some(&digest('a')), None).await;
    select_image(&db, "legacy").await;

    let rollout = ResizeRollout::new(
        db.clone(),
        signed_inventory(&[digest('a')]),
        durable_admission(&db),
        RecordingPodPlane::drained(),
        Arc::new(UnusedRegistry),
        RecordingPreflight::clear(),
    );

    // Under `leaf-v1`: an allowlisted no-handshake digest maps to leaf
    // authority, named by the digest it was vouched for at.
    let verdicts = rollout.validate_catalog(LEAF).await.unwrap();
    assert_eq!(
        verdicts,
        vec![(
            "legacy".to_owned(),
            CatalogVerdict::LegacyLeafAuthority(PreProtocolDigest::parse(&digest('a')).unwrap())
        )]
    );
    assert_eq!(verdicts[0].1.authority(), LEAF);

    // MUTATION 1 — drop the mode from the comparison and the same row passes
    // under `resize-v2`. It must not: `resize-v2` is not the behaviour a
    // no-handshake artifact was built against, allowlisted or otherwise.
    let blocked = rollout.validate_catalog(RESIZE).await.unwrap_err();
    let RolloutBlocked::CatalogIncompatible(defects) = blocked else {
        panic!("an allowlisted no-handshake row must be refused under resize-v2, got {blocked:?}");
    };
    assert_eq!(defects.len(), 1);
    assert_eq!(defects[0].image_id, "legacy");
    assert!(
        matches!(
            defects[0].reason,
            CatalogDefectReason::Rejected(AdmissionRejection::MissingDeclarationUnderMode { .. })
        ),
        "got {:?}",
        defects[0].reason
    );
}

/// A `resize-v2` declaration is refused under `leaf-v1` and admitted under
/// `resize-v2`, and the reverse holds for `leaf-v1`.
#[tokio::test]
async fn a_declaration_is_admitted_only_by_the_mode_that_matches_it() {
    let db = legacy_leaf_database().await;
    assert_real_postgres(&db).await;
    seed_project_and_run(&db, "019fb854-3333-7000-8000-000000000001").await;
    seed_image(&db, "modern", Some(&digest('c')), Some(RESIZE)).await;
    select_image(&db, "modern").await;

    let rollout = ResizeRollout::new(
        db.clone(),
        signed_inventory(&[digest('c')]),
        durable_admission(&db),
        RecordingPodPlane::drained(),
        Arc::new(UnusedRegistry),
        RecordingPreflight::clear(),
    );

    assert_eq!(
        rollout.validate_catalog(RESIZE).await.unwrap(),
        vec![("modern".to_owned(), CatalogVerdict::Declared(RESIZE))]
    );

    let blocked = rollout.validate_catalog(LEAF).await.unwrap_err();
    let RolloutBlocked::CatalogIncompatible(defects) = blocked else {
        panic!("a resize-v2 declaration must be refused under leaf-v1, got {blocked:?}");
    };
    assert_eq!(
        defects[0].reason,
        CatalogDefectReason::Rejected(AdmissionRejection::Declared(
            LauncherProtocolAdmission::ProtocolMismatch {
                mode: LEAF,
                declared: RESIZE
            }
        )),
        "the refusal must be the one z3gi owns, carried verbatim"
    );
    // Even though the inventory vouches for this exact digest: a declaration is
    // never reinterpreted by the legacy arm.
    assert!(
        matches!(
            rollout.validate_catalog(LEAF).await,
            Err(RolloutBlocked::CatalogIncompatible(_))
        ),
        "an allowlisted digest must not launder a mismatched declaration"
    );
}

/// MUTATION 2 — comparing wire strings instead of the enum lets an unknown
/// value such as `resize-v3` through as "not resize-v2, therefore leaf".
///
/// Reached through the pure classifier because migration 166's
/// `launcher_authority_protocol` CHECK makes the value unstorable, which is
/// asserted here too: the database is the outer fence and the parse is the
/// inner one, and both are proven rather than assumed.
#[tokio::test]
async fn an_unknown_protocol_string_is_refused_and_is_also_unstorable() {
    let db = legacy_leaf_database().await;
    assert_real_postgres(&db).await;
    seed_image(&db, "modern", Some(&digest('c')), Some(RESIZE)).await;

    let inventory = signed_inventory(&[digest('c')]);
    for mode in LauncherAuthorityProtocol::ALL {
        let unknown = Image {
            launcher_authority_protocol: Some("resize-v3".into()),
            ..ImageRepository::new(db.clone())
                .get("modern")
                .await
                .unwrap()
                .unwrap()
        };
        assert_eq!(
            classify_catalog_image(mode, &unknown, &inventory),
            Err(CatalogDefectReason::UnparseableDeclaration {
                declared: "resize-v3".into()
            }),
            "a wire-string comparison would read `resize-v3` as an authority under {mode}"
        );
    }

    // And the column refuses it outright, so the pure path above is the only
    // way the value can reach the decision at all.
    let refused = sqlx::query(
        "UPDATE images SET launcher_authority_protocol = 'resize-v3' WHERE id = 'modern'",
    )
    .execute(db.pool())
    .await;
    assert!(
        refused.is_err(),
        "migration 166 must refuse an unknown protocol at the column"
    );
}

// ═══ AC4 — the admission pause, proven by a refused dispatch ════════════════

/// The pause is asserted by dispatching and being refused, and by the Pod
/// creation count not moving.
#[tokio::test]
async fn the_admission_pause_is_proven_by_a_refused_dispatch() {
    let db = legacy_leaf_database().await;
    assert_real_postgres(&db).await;
    seed_project_and_run(&db, "019fb854-4444-7000-8000-000000000001").await;
    let image = seed_image(&db, "legacy", Some(&digest('a')), None).await;

    let pods = RecordingPodPlane::drained();
    let rollout = ResizeRollout::new(
        db.clone(),
        signed_inventory(&[digest('a')]),
        durable_admission(&db),
        Arc::clone(&pods) as Arc<dyn TaskRunPodPlane>,
        Arc::new(UnusedRegistry),
        RecordingPreflight::clear(),
    );

    // Positive control: unpaused, the same dispatch creates a Pod. Without this
    // the refusal below would be indistinguishable from a driver that refuses
    // everything.
    assert_eq!(
        rollout
            .attempt_dispatch("019fb854-4444-7000-8000-000000000001", &image)
            .await,
        DispatchOutcome::Dispatched {
            authority: LEAF,
            resized: false
        }
    );
    assert_eq!(pods.pod_creations(), 1);

    // Pause. `pause_admission` proves itself the same way this test does: it
    // dispatches and requires a refusal.
    rollout.verify_retention(&[]).await.unwrap();
    rollout
        .pause_admission("eeky cutover", &probe_for(&image))
        .await
        .expect("the pause must take effect, not merely be recorded");

    assert_eq!(
        rollout
            .attempt_dispatch("019fb854-4444-7000-8000-000000000002", &image)
            .await,
        DispatchOutcome::RefusedByAdmissionPause
    );
    assert_eq!(
        pods.pod_creations(),
        1,
        "a paused dispatch attempt must create no Pod; the assertion is the count, not the row"
    );
    assert_eq!(
        rollout.dispatches_admitted_while_paused(),
        0,
        "no Pod may be created between the pause and the resume"
    );
}

/// **The mutation, run.** A pause that writes the row and wires no refusal is
/// caught by the pause step itself, and the cutover stops there.
///
/// A test asserting `dispatch_pause_status() == paused` would pass against this
/// implementation. This one does not, which is the whole point.
#[tokio::test]
async fn a_pause_row_without_a_wired_refusal_blocks_the_cutover() {
    let db = legacy_leaf_database().await;
    assert_real_postgres(&db).await;
    seed_project_and_run(&db, "019fb854-5555-7000-8000-000000000001").await;
    let image = seed_image(&db, "legacy", Some(&digest('a')), None).await;

    let pods = RecordingPodPlane::drained();
    let broken = Arc::new(RowOnlyAdmissionControl {
        paused: Mutex::new(false),
    });
    let rollout = ResizeRollout::new(
        db.clone(),
        signed_inventory(&[digest('a')]),
        Arc::clone(&broken) as Arc<dyn AdmissionControl>,
        Arc::clone(&pods) as Arc<dyn TaskRunPodPlane>,
        Arc::new(UnusedRegistry),
        RecordingPreflight::clear(),
    );

    rollout.verify_retention(&[]).await.unwrap();
    assert_eq!(
        rollout
            .pause_admission("eeky cutover", &probe_for(&image))
            .await,
        Err(RolloutBlocked::AdmissionPauseIneffective)
    );
    assert!(
        *broken.paused.lock().unwrap(),
        "the row WAS written — a row-reading assertion would be green here"
    );
    assert_eq!(
        pods.pod_creations(),
        1,
        "the probe dispatch was admitted, which is exactly what makes the pause ineffective"
    );

    // And the cutover cannot proceed: the flip's prerequisite never entered the
    // journal, and the durable mode is untouched.
    assert!(!rollout.journal().contains(&RolloutStep::AdmissionPaused));
    assert_eq!(
        rollout.flip_authority_mode(0, RESIZE).await,
        Err(RolloutBlocked::StepOutOfOrder {
            step: RolloutStep::AuthorityModeFlipped,
            missing: RolloutStep::AdmissionPaused
        })
    );
    assert_eq!(authority(&db).await, (LEAF, 0));
}

// ═══ AC5 — both flips gated on both drain dimensions ════════════════════════

/// Build a rollout already advanced to the point where the next legal step is
/// the flip, with the given Pod plane.
async fn armed_at_the_flip(
    db: &Database,
    pods: Arc<RecordingPodPlane>,
    image: &Image,
    inventory: LegacyDigestInventory,
) -> ResizeRollout {
    let rollout = ResizeRollout::new(
        db.clone(),
        inventory,
        durable_admission(db),
        Arc::clone(&pods) as Arc<dyn TaskRunPodPlane>,
        Arc::new(UnusedRegistry),
        RecordingPreflight::clear(),
    );
    rollout.verify_retention(&[]).await.unwrap();
    rollout
        .pause_admission("eeky cutover", &probe_for(image))
        .await
        .unwrap();
    rollout
}

/// The four seeded-blocker cases, plus the proof that the block comes from
/// `list_nonterminal_resize` and not from `set_mode`'s own census.
#[tokio::test]
async fn both_flips_are_gated_on_zero_live_pods_and_zero_nonterminal_rows() {
    for (label, target) in [("forward", RESIZE), ("rollback", LEAF)] {
        // ── one nonterminal resize row blocks this flip ──────────────────
        {
            let db = legacy_leaf_database().await;
            assert_real_postgres(&db).await;
            let run = "019fb854-6666-7000-8000-000000000001";
            seed_project_and_run(&db, run).await;
            let image = seed_image(&db, "legacy", Some(&digest('a')), None).await;
            seed_nonterminal_resize_row(&db, run).await;

            let rollout = armed_at_the_flip(
                &db,
                RecordingPodPlane::drained(),
                &image,
                signed_inventory(&[digest('a')]),
            )
            .await;
            let blocked = rollout.prove_drained().await.unwrap_err();

            // The variant is the one only `list_nonterminal_resize` can
            // produce, and it names the row. Deleting that call would leave
            // `set_mode`'s fence to refuse the flip — with
            // `AuthorityDrainRefused` and a bare count, which this assertion
            // rejects.
            let RolloutBlocked::NonterminalResizeRows(rows) = blocked else {
                panic!("{label}: expected named nonterminal rows, got {blocked:?}");
            };
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].task_run_id, run);
            assert_eq!(rows[0].state, "BirthConfirmed");
            assert_eq!(
                rows[0].pod_uid.as_deref(),
                Some(format!("uid-{run}").as_str())
            );

            assert_eq!(
                rollout.flip_authority_mode(0, target).await,
                Err(RolloutBlocked::StepOutOfOrder {
                    step: RolloutStep::AuthorityModeFlipped,
                    missing: RolloutStep::DrainProven
                }),
                "{label}: an unproven drain must not be flippable"
            );
            assert_eq!(
                authority(&db).await,
                (LEAF, 0),
                "{label}: mode must not move"
            );
        }

        // ── one live task-run Pod blocks this flip ───────────────────────
        {
            let db = legacy_leaf_database().await;
            assert_real_postgres(&db).await;
            seed_project_and_run(&db, "019fb854-7777-7000-8000-000000000001").await;
            let image = seed_image(&db, "legacy", Some(&digest('a')), None).await;

            // PostgreSQL is fully drained here — `set_mode` would happily flip.
            assert!(
                BuildPodPermitRepository::new(db.clone())
                    .list_nonterminal_resize()
                    .await
                    .unwrap()
                    .is_empty()
            );

            let rollout = armed_at_the_flip(
                &db,
                RecordingPodPlane::with_live_pod(),
                &image,
                signed_inventory(&[digest('a')]),
            )
            .await;
            let blocked = rollout.prove_drained().await.unwrap_err();
            let RolloutBlocked::LiveTaskRunPods(live) = blocked else {
                panic!("{label}: a live Pod must block the flip, got {blocked:?}");
            };
            assert_eq!(live.len(), 1);
            assert_eq!(live[0].pod_name, "taskrun-still-running");
            assert_eq!(
                authority(&db).await,
                (LEAF, 0),
                "{label}: mode must not move"
            );
        }
    }
}

/// Forward and rollback both flip from a genuinely drained snapshot, so the
/// four refusals above are caused by the blockers and not by the fence being
/// stuck closed.
#[tokio::test]
async fn a_drained_snapshot_flips_forward_and_back() {
    let db = legacy_leaf_database().await;
    assert_real_postgres(&db).await;
    seed_project_and_run(&db, "019fb854-8888-7000-8000-000000000001").await;
    let image = seed_image(&db, "legacy", Some(&digest('a')), None).await;

    let forward = armed_at_the_flip(
        &db,
        RecordingPodPlane::drained(),
        &image,
        signed_inventory(&[digest('a')]),
    )
    .await;
    forward.prove_drained().await.unwrap();
    forward.clear_preflight(RESIZE).await.unwrap();
    assert_eq!(forward.flip_authority_mode(0, RESIZE).await, Ok(1));
    forward.resume_admission().await.unwrap();
    assert_eq!(authority(&db).await, (RESIZE, 1));

    let back = armed_at_the_flip(
        &db,
        RecordingPodPlane::drained(),
        &image,
        signed_inventory(&[digest('a')]),
    )
    .await;
    back.prove_drained().await.unwrap();
    back.clear_preflight(LEAF).await.unwrap();
    assert_eq!(back.flip_authority_mode(1, LEAF).await, Ok(2));
    back.resume_admission().await.unwrap();
    assert_eq!(authority(&db).await, (LEAF, 2));
}

// ═══ AC6 — rollback blocked, admission stays paused ═════════════════════════

/// Three unavailability shapes. In each: no flip, no resume, and — asserted by
/// a real dispatch attempt, not by a status read — admission is still refusing.
#[tokio::test]
async fn rollback_is_blocked_and_leaves_admission_paused() {
    // The three cases share a shape, so they share a driver body: seed, pause
    // admission through the production path, break exactly one thing, attempt
    // the rollback, then dispatch and require a refusal.
    for case in ["allowlist absent", "digest not pullable", "row repointed"] {
        let db = legacy_leaf_database().await;
        assert_real_postgres(&db).await;
        seed_project_and_run(&db, "019fb854-9999-7000-8000-000000000001").await;
        let registry = LocalRegistry::start().await;
        let manifest = format!("{{\"schemaVersion\":2,\"case\":\"{case}\"}}").into_bytes();
        let retained_digest = registry.push("djinn-image-legacy", &manifest);
        let image = seed_image(&db, "legacy", Some(&retained_digest), None).await;
        select_image(&db, "legacy").await;

        // The cutover is mid-flight: the mode is `resize-v2` and admission is
        // already paused. This is the state a failed forward attempt leaves.
        LauncherAuthorityModeRepository::new(db.clone())
            .set_mode(0, RESIZE)
            .await;
        assert_eq!(authority(&db).await, (RESIZE, 1));
        DispatchPauseRepository::new(db.clone(), EventBus::noop())
            .pause(
                djinn_db::DispatchPauseTarget::global(),
                djinn_core::models::DispatchPause {
                    paused_by: "eeky-cutover".into(),
                    paused_at: "2026-07-31T00:00:00Z".into(),
                    reason: "forward cutover failed".into(),
                    expires_at: None,
                },
            )
            .await
            .unwrap();

        let inventory = match case {
            // The signed allowlist file is absent from the deployment.
            "allowlist absent" => LegacyDigestInventory::Unconfigured,
            _ => signed_inventory(std::slice::from_ref(&retained_digest)),
        };
        if case == "digest not pullable" {
            registry.delete("djinn-image-legacy", &retained_digest);
        }
        if case == "row repointed" {
            // The catalog row now names a digest the retained set does not
            // contain and the inventory does not vouch for.
            sqlx::query("UPDATE images SET registry_digest = $1 WHERE id = 'legacy'")
                .bind(digest('f'))
                .execute(db.pool())
                .await
                .unwrap();
        }

        let pods = RecordingPodPlane::drained();
        let rollout = ResizeRollout::new(
            db.clone(),
            inventory,
            durable_admission(&db),
            Arc::clone(&pods) as Arc<dyn TaskRunPodPlane>,
            registry.probe(),
            RecordingPreflight::clear(),
        );
        let plan = RolloutPlan {
            retained: &[RetainedArtifact {
                image_id: "legacy".into(),
                repository: "djinn-image-legacy".into(),
                digest: retained_digest.clone(),
                role: RetentionRole::LeafV1Rollback,
            }],
            probe: probe_for(&image),
            expected_epoch: 1,
            reason: "rolling back",
        };

        let Err(blocked) = rollout.rollback(&plan).await else {
            panic!("{case}: rollback must be blocked");
        };
        match (case, &blocked) {
            ("allowlist absent", RolloutBlocked::AllowlistUnavailable(defect)) => {
                assert_eq!(*defect, LegacyAllowlistDefect::Unsigned);
            }
            ("digest not pullable", RolloutBlocked::RetentionUnprovable { digest, .. }) => {
                assert_eq!(*digest, retained_digest);
            }
            // The repointed row is a dispatch-eligible no-handshake image the
            // signed document does not vouch for, so the allowlist cross-check
            // names it before the catalog validation is even reached. The
            // distinguishing assertion is the row identity, which only a real
            // catalog read can supply.
            (
                "row repointed",
                RolloutBlocked::AllowlistUnavailable(LegacyAllowlistDefect::Uninventoried(rows)),
            ) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].image_id, "legacy");
                assert_eq!(rows[0].registry_digest, digest('f'));
            }
            _ => panic!("{case}: unexpected block {blocked:?}"),
        }

        // The mode did not flip. Read from PostgreSQL, not from the driver.
        assert_eq!(
            authority(&db).await,
            (RESIZE, 1),
            "{case}: a blocked rollback must not move the authority mode"
        );
        // No Pod was started, and no resize was PATCHed. Counted, not intended.
        assert_eq!(pods.pod_creations(), 0, "{case}: no Pod may be created");
        assert_eq!(pods.resize_patches(), 0, "{case}: no resize may be issued");
        // Admission is still refusing — proven by dispatching, not by reading a
        // status. `Err` alone would stay green if the mode flipped first and the
        // error came afterwards, which the mode assertion above also rejects.
        assert_eq!(
            rollout
                .attempt_dispatch("019fb854-9999-7000-8000-000000000002", &image)
                .await,
            DispatchOutcome::RefusedByAdmissionPause,
            "{case}: admission must stay paused"
        );
        assert_eq!(pods.pod_creations(), 0, "{case}: still no Pod");
        assert!(
            !rollout.journal().contains(&RolloutStep::AdmissionResumed),
            "{case}: admission must not have been resumed"
        );
        assert_eq!(rollout.dispatches_admitted_while_paused(), 0, "{case}");
    }
}

// ═══ AC7 — the ordering is enforced, not documented ═════════════════════════

/// The full forward sequence, asserted as an observed step sequence.
#[tokio::test]
async fn the_forward_cutover_records_the_specced_step_sequence() {
    let db = legacy_leaf_database().await;
    assert_real_postgres(&db).await;
    seed_project_and_run(&db, "019fb854-aaaa-7000-8000-000000000001").await;
    let registry = LocalRegistry::start().await;
    let manifest = br#"{"schemaVersion":2,"role":"resize-v2"}"#;
    let current = registry.push("djinn-image-modern", manifest);
    let image = seed_image(&db, "modern", Some(&current), Some(RESIZE)).await;
    select_image(&db, "modern").await;

    let pods = RecordingPodPlane::drained();
    let rollout = ResizeRollout::new(
        db.clone(),
        signed_inventory(&[digest('a')]),
        durable_admission(&db),
        Arc::clone(&pods) as Arc<dyn TaskRunPodPlane>,
        registry.probe(),
        RecordingPreflight::clear(),
    );
    let plan = RolloutPlan {
        retained: &[RetainedArtifact {
            image_id: "modern".into(),
            repository: "djinn-image-modern".into(),
            digest: current.clone(),
            role: RetentionRole::ResizeV2Current,
        }],
        probe: probe_for(&image),
        expected_epoch: 0,
        reason: "eeky cutover",
    };

    assert_eq!(rollout.activate(&plan).await, Ok(1));
    assert_eq!(
        rollout.journal(),
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
        "the observed sequence is the assertion; a checklist document is not"
    );
    assert_eq!(authority(&db).await, (RESIZE, 1));
}

/// The two reorderings the criterion names, each attempted for real.
#[tokio::test]
async fn the_flip_cannot_precede_the_drain_and_the_resume_cannot_precede_the_flip() {
    let db = legacy_leaf_database().await;
    assert_real_postgres(&db).await;
    seed_project_and_run(&db, "019fb854-bbbb-7000-8000-000000000001").await;
    let image = seed_image(&db, "legacy", Some(&digest('a')), None).await;

    let pods = RecordingPodPlane::drained();
    let rollout = armed_at_the_flip(
        &db,
        Arc::clone(&pods),
        &image,
        signed_inventory(&[digest('a')]),
    )
    .await;

    // REORDERING 1 — flip before the drain proof.
    assert_eq!(
        rollout.flip_authority_mode(0, RESIZE).await,
        Err(RolloutBlocked::StepOutOfOrder {
            step: RolloutStep::AuthorityModeFlipped,
            missing: RolloutStep::DrainProven
        })
    );
    assert_eq!(
        authority(&db).await,
        (LEAF, 0),
        "the mode must not have moved"
    );

    // REORDERING 2 — resume before the flip is confirmed.
    rollout.prove_drained().await.unwrap();
    assert_eq!(
        rollout.resume_admission().await,
        Err(RolloutBlocked::StepOutOfOrder {
            step: RolloutStep::AdmissionResumed,
            missing: RolloutStep::AuthorityModeFlipped
        })
    );
    // And admission really is still refusing — asserted by dispatching.
    assert_eq!(
        rollout
            .attempt_dispatch("019fb854-bbbb-7000-8000-000000000002", &image)
            .await,
        DispatchOutcome::RefusedByAdmissionPause
    );
    assert_eq!(
        pods.pod_creations(),
        0,
        "nothing has ever been dispatched here: even the pause probe was refused"
    );
    assert_eq!(rollout.dispatches_admitted_while_paused(), 0);

    // REORDERING 3 — flip with the drain proven but the preflight never run.
    // The gate is the prerequisite graph, not a call somebody remembered to
    // make: deleting `clear_preflight` from `activate` does not produce an
    // ungated flip, it produces this.
    assert_eq!(
        rollout.flip_authority_mode(0, RESIZE).await,
        Err(RolloutBlocked::StepOutOfOrder {
            step: RolloutStep::AuthorityModeFlipped,
            missing: RolloutStep::PreflightCleared
        })
    );
    assert_eq!(
        authority(&db).await,
        (LEAF, 0),
        "the mode must not have moved"
    );

    // In order, all three succeed.
    rollout.clear_preflight(RESIZE).await.unwrap();
    assert_eq!(rollout.flip_authority_mode(0, RESIZE).await, Ok(1));
    rollout.resume_admission().await.unwrap();
}

/// **A blocked preflight blocks the FLIP, not just the call.**
///
/// The seam here is injected, so what this proves is the *ordering* half: a
/// refusal leaves the durable mode and the epoch exactly where they were, and
/// leaves admission paused, because `resume_admission` requires a journaled
/// flip and the flip requires a journaled preflight. The half this cannot prove
/// — that the preflight production composes is the real one — is proven in
/// `tests/authority_cutover.rs`, which drives
/// `djinn_k8s::cutover_preflight::run` over a live render through
/// `ResizeRollout::production`.
#[tokio::test]
async fn a_blocked_preflight_leaves_the_mode_and_the_epoch_where_they_were() {
    let db = legacy_leaf_database().await;
    assert_real_postgres(&db).await;
    seed_project_and_run(&db, "019fb854-cccc-7000-8000-000000000001").await;
    let image = seed_image(&db, "legacy", Some(&digest('a')), None).await;

    let pods = RecordingPodPlane::drained();
    let preflight = RecordingPreflight::clear();
    let rollout = ResizeRollout::new(
        db.clone(),
        signed_inventory(&[digest('a')]),
        durable_admission(&db),
        Arc::clone(&pods) as Arc<dyn TaskRunPodPlane>,
        Arc::new(UnusedRegistry),
        Arc::clone(&preflight) as Arc<dyn CutoverPreflight>,
    );
    rollout.verify_retention(&[]).await.unwrap();
    rollout
        .pause_admission("eeky cutover", &probe_for(&image))
        .await
        .unwrap();
    rollout.prove_drained().await.unwrap();

    for (case, verdict) in [
        (
            "blocked",
            PreflightVerdict::Blocked {
                classes: vec!["pods-resize-rbac".into()],
                defects: vec!["pods-resize-rbac no Role grants the exact triple".into()],
            },
        ),
        (
            "unevaluable",
            PreflightVerdict::Unevaluable("the render could not be read".into()),
        ),
        // "No defects" and "no checks ran" must not be the same verdict. A
        // preflight that quietly stopped evaluating would otherwise return the
        // cleanest answer in the system.
        (
            "vacuous",
            PreflightVerdict::Clear {
                evaluated: Vec::new(),
            },
        ),
    ] {
        preflight.set(verdict);
        let calls_before = preflight.calls();
        let blocked = rollout
            .clear_preflight(RESIZE)
            .await
            .expect_err("a non-clean preflight must block");
        assert!(
            matches!(
                blocked,
                RolloutBlocked::PreflightRefused { .. }
                    | RolloutBlocked::PreflightUnevaluable(_)
                    | RolloutBlocked::PreflightVacuous
            ),
            "{case}: {blocked:?}"
        );
        assert_eq!(
            preflight.calls(),
            calls_before + 1,
            "{case}: the preflight must actually have been consulted"
        );
        assert!(
            !rollout.journal().contains(&RolloutStep::PreflightCleared),
            "{case}: a blocked preflight must not journal a cleared step"
        );

        // THE ASSERTION THAT MATTERS: not that an error came back, but that the
        // durable authority row is untouched. An error returned *after* a flip
        // would satisfy the first and fail this.
        assert_eq!(
            authority(&db).await,
            (LEAF, 0),
            "{case}: the mode and the epoch must not have moved"
        );
        assert_eq!(
            rollout
                .attempt_dispatch("019fb854-cccc-7000-8000-000000000002", &image)
                .await,
            DispatchOutcome::RefusedByAdmissionPause,
            "{case}: admission must be left paused"
        );
        assert_eq!(pods.pod_creations(), 0, "{case}");
    }

    // And with the flip still unreachable, so is the resume.
    assert_eq!(
        rollout.resume_admission().await,
        Err(RolloutBlocked::StepOutOfOrder {
            step: RolloutStep::AdmissionResumed,
            missing: RolloutStep::AuthorityModeFlipped
        })
    );
}

/// A step cannot run twice, so a replayed cutover cannot launder a fence.
#[tokio::test]
async fn no_step_runs_twice() {
    let db = legacy_leaf_database().await;
    assert_real_postgres(&db).await;
    let rollout = ResizeRollout::new(
        db.clone(),
        signed_inventory(&[]),
        durable_admission(&db),
        RecordingPodPlane::drained(),
        Arc::new(UnusedRegistry),
        RecordingPreflight::clear(),
    );
    rollout.freeze_catalog_mutation().unwrap();
    assert_eq!(
        rollout.freeze_catalog_mutation(),
        Err(RolloutBlocked::StepAlreadyRun(
            RolloutStep::CatalogMutationFrozen
        ))
    );
}

// ═══ AC8 — never two quota authorities, never none ══════════════════════════

/// The four (mode, artifact) combinations, asserted on counted Pod creations
/// and counted `pods/resize` PATCHes.
#[tokio::test]
async fn no_pod_ever_has_two_quota_authorities_or_none() {
    let db = legacy_leaf_database().await;
    assert_real_postgres(&db).await;
    seed_project_and_run(&db, "019fb854-cccc-7000-8000-000000000001").await;
    let legacy = seed_image(&db, "legacy", Some(&digest('a')), None).await;
    let modern = seed_image(&db, "modern", Some(&digest('c')), Some(RESIZE)).await;
    select_image(&db, "legacy").await;

    let pods = RecordingPodPlane::drained();
    let rollout = ResizeRollout::new(
        db.clone(),
        signed_inventory(&[digest('a')]),
        durable_admission(&db),
        Arc::clone(&pods) as Arc<dyn TaskRunPodPlane>,
        Arc::new(UnusedRegistry),
        RecordingPreflight::clear(),
    );

    // ── mode `leaf-v1` ───────────────────────────────────────────────────
    assert_eq!(authority(&db).await.0, LEAF);

    // A `resize-v2` image is cataloged but does not dispatch: no Pod at all.
    assert!(matches!(
        rollout
            .attempt_dispatch("019fb854-cccc-7000-8000-000000000002", &modern)
            .await,
        DispatchOutcome::RefusedByCatalog(_)
    ));
    assert_eq!(
        pods.pod_creations(),
        0,
        "permitting a resize-v2 image to dispatch under leaf-v1 must create no Pod"
    );

    // An allowlisted no-handshake image retains launcher leaf authority and is
    // never resized.
    assert_eq!(
        rollout
            .attempt_dispatch("019fb854-cccc-7000-8000-000000000003", &legacy)
            .await,
        DispatchOutcome::Dispatched {
            authority: LEAF,
            resized: false
        }
    );
    assert_eq!(pods.pod_creations(), 1);
    assert_eq!(
        pods.resize_patches(),
        0,
        "a resize PATCH against a leaf-v1 Pod would be the second quota writer"
    );

    // ── mode `resize-v2` ─────────────────────────────────────────────────
    LauncherAuthorityModeRepository::new(db.clone())
        .set_mode(0, RESIZE)
        .await;
    assert_eq!(authority(&db).await.0, RESIZE);

    // The allowlisted no-handshake image now dispatches nowhere: `resize-v2`
    // is not the behaviour it was built against.
    assert!(matches!(
        rollout
            .attempt_dispatch("019fb854-cccc-7000-8000-000000000004", &legacy)
            .await,
        DispatchOutcome::RefusedByCatalog(_)
    ));
    assert_eq!(pods.pod_creations(), 1, "no new Pod");
    assert_eq!(pods.resize_patches(), 0);

    // And the `resize-v2` image dispatches with exactly one resize PATCH.
    assert_eq!(
        rollout
            .attempt_dispatch("019fb854-cccc-7000-8000-000000000005", &modern)
            .await,
        DispatchOutcome::Dispatched {
            authority: RESIZE,
            resized: true
        }
    );
    assert_eq!(pods.pod_creations(), 2);
    assert_eq!(pods.resize_patches(), 1);
}

// ═══ reachability and retention gates ═══════════════════════════════════════

fn repository_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .expect("the server manifest sits one level below the checkout root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = repository_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {path:?}: {error}"))
}

/// `text` with whole-line comments removed.
///
/// Copied from `server/tests/task_run_resize_kind.rs`, whose doc comment is the
/// canonical statement of this defect class. These are separate test binaries,
/// so the helper is duplicated rather than shared. A comment cannot construct a
/// value, call a method or gate a module, so it must neither satisfy a positive
/// assertion nor trip a negative one — and both directions were live in the
/// gates below:
///
/// * **False negative.** `launcher_owns_leaf_quota` is named twice in comments
///   in `djinn-k8s/src/launcher.rs` and once in code. `cgroup.kill` and
///   `wait_empty` are named together in one comment in the cgroup launcher.
///   Deleting the code left every one of those retention gates green.
/// * **False positive.** `!driver.contains("kube::")` and
///   `!driver.contains("#[cfg(test)]")` fired on any comment that so much as
///   mentioned the thing it was there to forbid.
///
/// Only WHOLE-line comments are dropped, deliberately: a trailing comment must
/// not be able to launder the code in front of it.
fn code_lines(text: &str, comment_prefix: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with(comment_prefix))
        .collect::<Vec<_>>()
        .join("\n")
}

/// [`code_lines`] for Rust. Covers `//`, `///` and `//!`.
fn rust_code(source: &str) -> String {
    code_lines(source, "//")
}

/// Whether `needle` appears in `source`'s Rust CODE.
///
/// Split out from its guards so their logic is testable against synthetic
/// input: otherwise "ignores comments" and "ignores everything" are
/// indistinguishable from a green run.
fn code_contains(source: &str, needle: &str) -> bool {
    rust_code(source).contains(needle)
}

/// The retention needles of `retained` that `source` no longer carries in CODE.
fn retention_needles_missing_from<'a>(source: &str, retained: &[&'a str]) -> Vec<&'a str> {
    let code = rust_code(source);
    retained
        .iter()
        .copied()
        .filter(|needle| !code.contains(needle))
        .collect()
}

/// **The production composition site.**
///
/// The reachability question this repository keeps losing to is not "does the
/// code exist" but "is the thing production composes the thing the tests
/// drove". `ResizeRollout::production` is that site, and it is asserted here to
/// wire only production implementations — the durable dispatch-pause state, the
/// live apiserver, an HTTP registry — through the same `Self::new` the tests
/// call. It is unconditional: no feature gate, no `cfg`, no fallback branch.
///
/// The type-level half of this is stronger than the textual half and is not
/// restated here: `ResizeRollout::new` takes a `Database`, so no production
/// path *can* be handed a substitute for the drain fence or the catalog.
#[test]
fn the_production_composition_site_wires_only_production_implementations() {
    // Comment-stripped in both directions: a doc comment must neither wire a
    // production implementation nor make the constructor conditional.
    let driver = rust_code(&read("server/src/task_run_resize_rollout.rs"));
    let start = driver
        .find("pub fn production(")
        .expect("the module must expose a production composition site");
    let body = &driver[start..];
    let end = body.find("\n}\n").expect("the constructor must terminate");
    let body = &body[..end];

    for required in [
        "DurableAdmissionControl::new",
        "KubernetesTaskRunPodPlane::new",
        "HttpRegistryProbe::new",
        "LegacyDigestInventory::process()",
        // The preflight the flip is gated on must be the REAL one, assembled
        // from a live render by the same module `bin/cutover-preflight.rs`
        // uses. A production constructor that accepted an
        // `Arc<dyn CutoverPreflight>` would be a seam through which a fake
        // verdict could reach the flip.
        "DeployRenderPreflight::load",
        "Self::new(",
    ] {
        assert!(
            body.contains(required),
            "the production constructor must wire {required}, got:\n{body}"
        );
    }
    for forbidden in ["cfg(", "feature =", "unimplemented", "todo!"] {
        assert!(
            !body.contains(forbidden),
            "{forbidden:?} in the production constructor would make it conditional"
        );
    }

    // The Kubernetes Pod plane composes production `djinn-k8s` entry points and
    // introduces no primitive of its own.
    for required in [
        ".list_taskrun_jobs()",
        "TaskRunPodResizeSurface::from_runtime",
        ".observe_launcher(",
    ] {
        assert!(
            driver.contains(required),
            "the live-Pod census must go through the production entry point {required}"
        );
    }
    // …and names no `kube` type of its own, so it cannot diverge from what
    // dispatch and bootstrap already observe.
    assert!(
        !driver.contains("kube::"),
        "the Pod plane must compose djinn-k8s, not reach past it"
    );
}

/// **The reachability gate.**
///
/// `list_nonterminal_resize` had no production caller before this module. The
/// tests above drive it, but a test can drive anything — what makes it
/// *reachable* is that the call sits in non-test server code that the crate
/// compiles unconditionally, reached from a driver whose durable half has no
/// substitutable seam.
#[test]
fn the_drain_proof_calls_list_nonterminal_resize_from_compiled_server_code() {
    // The driver's own module docs discuss `list_nonterminal_resize` at length,
    // so only a call in CODE counts — and the `cfg(test)` ban must not fire on a
    // comment that merely explains why there is no test-only path.
    let driver = read("server/src/task_run_resize_rollout.rs");
    assert!(
        code_contains(&driver, ".list_nonterminal_resize()"),
        "the drain proof must call the production repository method"
    );
    assert!(
        !code_contains(&driver, "#[cfg(test)]"),
        "the driver must carry no test-only code path; the drain proof one production \
         caller compiles is the same one the tests drive"
    );

    let lib = rust_code(&read("server/src/lib.rs"));
    assert!(
        lib.contains("pub mod task_run_resize_rollout;"),
        "the module must be declared in the server crate, unconditionally"
    );
    assert!(
        !lib.contains("#[cfg(test)]\npub mod task_run_resize_rollout;"),
        "the module must not be gated behind cfg(test)"
    );
}

/// The durable half of the driver has no injection seam, so no fake repository
/// can stand in for the drain fence or the catalog check.
#[test]
fn the_driver_exposes_no_seam_for_a_fake_repository() {
    let driver = rust_code(&read("server/src/task_run_resize_rollout.rs"));
    // Needles are assembled at compile time so this file does not match itself.
    for forbidden in [
        concat!("permits: ", "impl"),
        concat!("permits: ", "Arc<dyn"),
        concat!("authority: ", "Arc<dyn"),
        concat!("images: ", "Arc<dyn"),
        concat!("trait ", "BuildPodPermits"),
        concat!("trait ", "ImageCatalog"),
    ] {
        assert!(
            !driver.contains(forbidden),
            "{forbidden:?} would make the durable half substitutable"
        );
    }
    assert!(
        driver.contains("BuildPodPermitRepository::new(db.clone())"),
        "the permit repository must be constructed from the Database, not injected"
    );

    // Assembled at compile time and comment-stripped: this file reads ITSELF, so
    // a needle spelled on its own assertion line is satisfied by that line and
    // can never fail — and the module doc at the top of this file names the
    // constructor too.
    let tests = rust_code(&read("server/tests/task_run_resize_rollout.rs"));
    assert!(
        tests.contains(concat!("Database::", "ephemeral()")),
        "these tests must construct a real PgPool"
    );
    for forbidden in [
        concat!("Fake", "Permit"),
        concat!("Mock", "Permit"),
        concat!("Fake", "Image"),
        concat!("Mock", "Image"),
        concat!("Fake", "Database"),
    ] {
        assert!(
            !tests.contains(forbidden),
            "{forbidden:?} is forbidden here"
        );
    }
}

/// **The retention gate (AC9), such as it can be until the repeated-cycle task
/// lands its own.**
///
/// This task reintroduces no blanket launcher CPU clamp and retires nothing.
/// The assertions below are structural, so the claim is checked rather than
/// asserted in a PR description: the ceiling render is still conditioned on
/// `resize-v2`, and every protected asset still exists.
#[test]
fn no_blanket_launcher_cpu_clamp_is_reintroduced_and_nothing_is_retired() {
    // The driver renders nothing at all: no manifest type, no resource block,
    // no quantity. It cannot produce a container `limits.cpu` because it never
    // constructs a container.
    let driver = rust_code(&read("server/src/task_run_resize_rollout.rs"));
    for rendering in [
        "ResourceRequirements",
        "k8s_openapi",
        "Quantity",
        "\"limits\"",
        "initContainers",
    ] {
        assert!(
            !driver.contains(rendering),
            "{rendering:?} would mean this module renders a container spec"
        );
    }

    // Outbound HTTP goes through the capability owner. `scripts/check-http-boundary.sh`
    // enforces this repository-wide; asserting it here too means a reviewer sees
    // the constraint beside the code it constrains rather than only in CI.
    for forbidden in [concat!("reqwest", "::"), concat!("hyper", "::")] {
        assert!(
            !driver.contains(forbidden),
            "{forbidden:?}: outbound HTTP must go through djinn_provider::http_util"
        );
    }
    assert!(
        driver.contains("djinn_provider::http_util::HttpClient"),
        "the registry probe must use the capability owner's client"
    );

    // The `resize-v2`-only condition on the ceiling render is intact. Read from
    // CODE: that file names `launcher_owns_leaf_quota` twice in comments and
    // once in the `if`, so a raw `contains` survives deleting the condition.
    let launcher = read("server/crates/djinn-k8s/src/launcher.rs");
    assert!(
        code_contains(&launcher, "launcher_owns_leaf_quota"),
        "the ceiling render must still be conditioned on the protocol"
    );

    // Protected assets still exist.
    for asset in [
        "server/crates/djinn-cgroup-launcher",
        "server/crates/djinn-k8s/src/pod_resize.rs",
        "server/crates/djinn-k8s/src/launcher.rs",
    ] {
        assert!(
            repository_root().join(asset).exists(),
            "{asset} must not be deleted by this task"
        );
    }
    // Both of these are named together in ONE comment in that file, explaining
    // that teardown is best-effort because `cgroup.kill` is asynchronous. That
    // comment alone satisfied a raw `contains` for both needles, so the
    // containment retention gate survived deleting the containment.
    let launcher_src = read("server/crates/djinn-cgroup-launcher/src/lib.rs");
    let missing = retention_needles_missing_from(&launcher_src, &["cgroup.kill", "wait_empty"]);
    assert!(
        missing.is_empty(),
        "{missing:?} must not be disabled by this task"
    );
}

// ═══ the gates' own self-tests ══════════════════════════════════════════════
//
// A source gate that ignores comments and a source gate that ignores everything
// look identical from a green run, so the predicate the gates above rest on is
// driven against synthetic input here, in BOTH directions.
//
// Two mutations, not one. A PRESENCE gate is mutated by REMOVING the required
// construct; swapping it for a different valid token reds the presence arm and
// proves nothing about a ban. A BAN is mutated by ADDING the banned token while
// LEAVING the required one in place.

#[test]
fn a_whole_line_comment_is_not_code_but_a_trailing_one_does_not_launder_the_line() {
    assert_eq!(rust_code("a\n// b\n  /// c\n//! d\ne"), "a\ne");
    assert_eq!(
        rust_code("    self.kill(); // `cgroup.kill` is asynchronous"),
        "    self.kill(); // `cgroup.kill` is asynchronous",
        "a trailing comment must NOT drop the code in front of it"
    );
}

#[test]
fn a_retention_gate_reads_the_code_it_protects_and_not_the_comment_about_it() {
    // This is the shape measured in `djinn-cgroup-launcher/src/lib.rs`: one
    // comment names both retained primitives, and the code names them again.
    let comment_only = "        // Teardown is best-effort ON PURPOSE. `cgroup.kill` is\n\
                        // asynchronous, so `wait_empty` can still observe a live pid.\n\
                        fn teardown(&mut self) {}\n";
    let with_code = "        // Teardown is best-effort ON PURPOSE. `cgroup.kill` is\n\
                     // asynchronous, so `wait_empty` can still observe a live pid.\n\
                     self.fs.write_leaf(leaf.fd, \"cgroup.kill\", \"1\")?;\n\
                     pub fn wait_empty(&mut self, leaf: &Leaf) {}\n";

    assert!(
        comment_only.contains("cgroup.kill") && comment_only.contains("wait_empty"),
        "the raw text a `contains` gate reads is satisfied by the comment alone — this is the \
         false negative the gate had"
    );
    assert_eq!(
        retention_needles_missing_from(comment_only, &["cgroup.kill", "wait_empty"]),
        vec!["cgroup.kill", "wait_empty"],
        "with the code deleted, BOTH retention needles must be reported missing"
    );
    assert!(
        retention_needles_missing_from(with_code, &["cgroup.kill", "wait_empty"]).is_empty(),
        "the shipped shape — comment and code together — must stay green"
    );
    assert!(
        retention_needles_missing_from(
            "self.kill_leaf(\"cgroup.kill\"); // retained\npub fn wait_empty() {} // retained",
            &["cgroup.kill", "wait_empty"]
        )
        .is_empty(),
        "a trailing comment must not launder the code in front of it"
    );
}

#[test]
fn a_reachability_gate_ignores_comments_in_both_directions() {
    let condition =
        "    if protocol.launcher_owns_leaf_quota() {\n        render_ceiling();\n    }\n";
    let prose = "/// ([`LauncherAuthorityProtocol::launcher_owns_leaf_quota`]), so a container\n";

    // FALSE NEGATIVE: prose alone must not satisfy the presence gate.
    assert!(
        prose.contains("launcher_owns_leaf_quota"),
        "the raw text matches"
    );
    assert!(
        !code_contains(prose, "launcher_owns_leaf_quota"),
        "deleting the `if` and keeping the doc comment must red the gate"
    );
    assert!(code_contains(
        &format!("{prose}{condition}"),
        "launcher_owns_leaf_quota"
    ));

    // FALSE POSITIVE: a comment naming a banned construct must not fire the ban,
    // while the real construct still must. The required call stays in place in
    // both fixtures so it is only ever the ban that moves.
    let call = "    let rows = repo.list_nonterminal_resize().await?;\n";
    let excused = "// This module reaches no `kube::` type of its own and carries no\n\
                   // #[cfg(test)] path.\n";
    assert!(
        code_contains(&format!("{call}{excused}"), ".list_nonterminal_resize()"),
        "the presence arm must hold under the ban's mutation, or the arms are confounded"
    );
    for banned in ["kube::", "#[cfg(test)]"] {
        assert!(
            !code_contains(&format!("{call}{excused}"), banned),
            "{banned:?} named only in a comment must not fire the ban"
        );
        assert!(
            code_contains(&format!("{call}{excused}    use {banned}Client;\n"), banned),
            "{banned:?} in real code must still fire the ban"
        );
    }
}
