//! PostgreSQL integration coverage for mixed-version typed-evidence authority.
//!
//! Every assertion below reads persisted rows; this deliberately does not use
//! fixture case names as a proxy for repository behavior.

use djinn_core::models::TribunalEvidenceLifecycle;
use djinn_db::{
    AppendTypedEvidenceTransitionInput, Database, DemandTypedEvidenceInput,
    TypedEvidenceRepository, legacy_demand_hash,
};
use serde_json::{Value, json};

struct Seed {
    proposal: String,
    creator: String,
    spike: String,
}

type FindingRow = (String, String, String, String, Value, i32, String);
type AttemptRow = (String, String, i32, String);
type TransitionRow = (String, i32, Option<String>, String, Option<String>, Value);

/// Read the actual authority records.  Counts alone cannot detect an incorrect
/// actor, ordinal, or binding in an otherwise plausible backfill.
async fn persisted_rows(
    db: &Database,
    proposal: &str,
) -> (Vec<FindingRow>, Vec<AttemptRow>, Vec<TransitionRow>) {
    let findings = sqlx::query_as("SELECT id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id FROM typed_evidence_findings WHERE proposal_id=$1 ORDER BY id")
        .bind(proposal).fetch_all(db.pool()).await.unwrap();
    let attempts = sqlx::query_as("SELECT id,finding_id,sequence,spike_task_id FROM typed_evidence_attempts WHERE finding_id IN (SELECT id FROM typed_evidence_findings WHERE proposal_id=$1) ORDER BY sequence")
        .bind(proposal).fetch_all(db.pool()).await.unwrap();
    let transitions = sqlx::query_as("SELECT id,ordinal,from_lifecycle,to_lifecycle,actor_task_id,metadata FROM typed_evidence_transitions WHERE finding_id IN (SELECT id FROM typed_evidence_findings WHERE proposal_id=$1) ORDER BY ordinal")
        .bind(proposal).fetch_all(db.pool()).await.unwrap();
    (findings, attempts, transitions)
}
async fn legacy_row(db: &Database, proposal: &str) -> (Option<String>, Option<String>) {
    sqlx::query_as("SELECT linked_spike_task_id,needs_evidence_claim FROM proposals WHERE id=$1")
        .bind(proposal)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn insert_task(db: &Database, project: &str, user: &str, title: &str) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,labels,acceptance_criteria,memory_refs,created_by_user_id) VALUES ($1,$2,$3,$4,'','','[]','[]','[]',$5)")
        .bind(&id)
        .bind(project)
        .bind(id.replace('-', ""))
        .bind(title)
        .bind(user)
        .execute(db.pool())
        .await
        .unwrap();
    id
}

async fn seed(db: &Database) -> Seed {
    let project = uuid::Uuid::now_v7().to_string();
    let user = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO projects (id,name,github_owner,github_repo) VALUES ($1,$2,'owner',$3)",
    )
    .bind(&project)
    .bind(format!("backfill-{project}"))
    .bind(format!("repo-{project}"))
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO users (id,github_id,github_login) VALUES ($1,$2,$3)")
        .bind(&user)
        .bind((uuid::Uuid::now_v7().as_u128() & i64::MAX as u128) as i64)
        .bind(format!("u-{user}"))
        .execute(db.pool())
        .await
        .unwrap();
    let creator = insert_task(db, &project, &user, "judge").await;
    let spike = insert_task(db, &project, &user, "spike").await;
    let proposal = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO proposals (id,short_id,title,body,body_format,acceptance_criteria,status,latest_revision_seq) VALUES ($1,$2,'p','','markdown','[]','draft',7)")
        .bind(&proposal).bind(proposal.replace('-', "")).execute(db.pool()).await.unwrap();
    Seed {
        proposal,
        creator,
        spike,
    }
}
fn claim(s: &Seed) -> Value {
    json!({"created_by_task_id":s.creator,"against_revision_seq":7,"uncertainty":"load-bearing"})
}
async fn legacy(db: &Database, s: &Seed, link: Option<&str>, claim: Option<&Value>) {
    sqlx::query("UPDATE proposals SET linked_spike_task_id=$1,needs_evidence_claim=$2 WHERE id=$3")
        .bind(link)
        .bind(claim.map(|v| serde_json::to_string(v).unwrap()))
        .bind(&s.proposal)
        .execute(db.pool())
        .await
        .unwrap();
}
async fn counts(db: &Database, proposal: &str) -> (i64, i64, i64) {
    let f: i64 =
        sqlx::query_scalar("SELECT count(*) FROM typed_evidence_findings WHERE proposal_id=$1")
            .bind(proposal)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let a: i64 = sqlx::query_scalar("SELECT count(*) FROM typed_evidence_attempts WHERE finding_id IN (SELECT id FROM typed_evidence_findings WHERE proposal_id=$1)").bind(proposal).fetch_one(db.pool()).await.unwrap();
    let t: i64 = sqlx::query_scalar("SELECT count(*) FROM typed_evidence_transitions WHERE finding_id IN (SELECT id FROM typed_evidence_findings WHERE proposal_id=$1)").bind(proposal).fetch_one(db.pool()).await.unwrap();
    (f, a, t)
}

type PersistedFinding = (String, String, String, String, Value, i32, String);
type PersistedAttempt = (String, String, i32, String);
type PersistedTransition = (String, i32, Option<String>, String, Option<String>, Value);
async fn persisted_rows(
    db: &Database,
    proposal: &str,
) -> (
    Vec<PersistedFinding>,
    Vec<PersistedAttempt>,
    Vec<PersistedTransition>,
) {
    let f = sqlx::query_as("SELECT id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id FROM typed_evidence_findings WHERE proposal_id=$1 ORDER BY id").bind(proposal).fetch_all(db.pool()).await.unwrap();
    let a = sqlx::query_as("SELECT id,finding_id,sequence,spike_task_id FROM typed_evidence_attempts WHERE finding_id IN (SELECT id FROM typed_evidence_findings WHERE proposal_id=$1) ORDER BY sequence").bind(proposal).fetch_all(db.pool()).await.unwrap();
    let t = sqlx::query_as("SELECT id,ordinal,from_lifecycle,to_lifecycle,actor_task_id,metadata FROM typed_evidence_transitions WHERE finding_id IN (SELECT id FROM typed_evidence_findings WHERE proposal_id=$1) ORDER BY ordinal").bind(proposal).fetch_all(db.pool()).await.unwrap();
    (f, a, t)
}
async fn legacy_row(db: &Database, proposal: &str) -> (Option<String>, Option<String>) {
    sqlx::query_as("SELECT linked_spike_task_id,needs_evidence_claim FROM proposals WHERE id=$1")
        .bind(proposal)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn typed_evidence_backfill_postgres_parity_matrix() {
    let db = Database::ephemeral().await.unwrap();
    db.ensure_initialized().await.unwrap();
    let typed = TypedEvidenceRepository::new(db.clone());

    // Claim-only backfill has canonical claim/hash and only the initial demand fact.
    let claim_only = seed(&db).await;
    let c = claim(&claim_only);
    legacy(&db, &claim_only, None, Some(&c)).await;
    let authority = legacy_row(&db, &claim_only.proposal).await;
    let report = typed.backfill_active_legacy_evidence().await.unwrap();
    assert_eq!(report.created_findings, 1);
    let projection = typed
        .dual_read_legacy_parity(&claim_only.proposal)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(projection.finding.claim, c);
    assert_eq!(
        projection.finding.lifecycle,
        TribunalEvidenceLifecycle::Demanded
    );
    assert_eq!(
        projection.finding.demand_hash,
        legacy_demand_hash(&projection.finding.claim, None)
    );
    assert_eq!(counts(&db, &claim_only.proposal).await, (1, 0, 1));
    let (f, a, t) = persisted_rows(&db, &claim_only.proposal).await;
    assert_eq!(
        (
            f[0].1.as_str(),
            f[0].2.as_str(),
            f[0].3.as_str(),
            &f[0].4,
            f[0].5,
            f[0].6.as_str()
        ),
        (
            claim_only.proposal.as_str(),
            legacy_demand_hash(&c, None).as_str(),
            "demanded",
            &c,
            7,
            claim_only.creator.as_str()
        )
    );
    assert!(a.is_empty());
    assert_eq!(
        (
            t[0].1,
            t[0].2.as_deref(),
            t[0].3.as_str(),
            t[0].4.as_deref(),
            &t[0].5
        ),
        (
            1,
            None,
            "demanded",
            Some(claim_only.creator.as_str()),
            &json!({"source":"legacy_backfill"})
        )
    );
    assert_eq!(legacy_row(&db, &claim_only.proposal).await, authority);

    // Link-only has its documented synthetic claim and a current active attempt.
    let link_only = seed(&db).await;
    legacy(&db, &link_only, Some(&link_only.spike), None).await;
    let authority = legacy_row(&db, &link_only.proposal).await;
    typed.backfill_active_legacy_evidence().await.unwrap();
    let p = typed
        .dual_read_legacy_parity(&link_only.proposal)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.spike_task_id.as_deref(), Some(link_only.spike.as_str()));
    assert!(p.attempt_id.is_some());
    assert_eq!(p.finding.lifecycle, TribunalEvidenceLifecycle::SpikeActive);
    assert_eq!(counts(&db, &link_only.proposal).await, (1, 1, 2));
    let (f, a, t) = persisted_rows(&db, &link_only.proposal).await;
    let synthetic = json!({"__typed_evidence_legacy_link_only":true});
    assert_eq!(
        (&f[0].4, f[0].2.as_str(), f[0].3.as_str()),
        (
            &synthetic,
            legacy_demand_hash(&synthetic, Some(&link_only.spike)).as_str(),
            "spike_active"
        )
    );
    assert_eq!(
        (a[0].1.as_str(), a[0].2, a[0].3.as_str()),
        (f[0].0.as_str(), 1, link_only.spike.as_str())
    );
    assert_eq!(
        t.iter()
            .map(|r| (r.1, r.2.clone(), r.3.clone(), r.4.clone(), r.5.clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                1,
                None,
                "demanded".into(),
                Some(link_only.spike.clone()),
                json!({"source":"legacy_backfill"})
            ),
            (
                2,
                Some("demanded".into()),
                "spike_active".into(),
                Some(link_only.spike.clone()),
                json!({"source":"legacy_backfill"})
            )
        ]
    );
    assert_eq!(legacy_row(&db, &link_only.proposal).await, authority);

    // Claim+link preserves legacy authority byte-for-byte and is idempotent.
    let both = seed(&db).await;
    let c = claim(&both);
    legacy(&db, &both, Some(&both.spike), Some(&c)).await;
    typed.backfill_active_legacy_evidence().await.unwrap();
    let before = counts(&db, &both.proposal).await;
    let rows_before = persisted_rows(&db, &both.proposal).await;
    let id = typed
        .dual_read_legacy_parity(&both.proposal)
        .await
        .unwrap()
        .unwrap()
        .finding
        .id;
    let second = typed.backfill_active_legacy_evidence().await.unwrap();
    assert_eq!((second.created_findings, second.created_attempts), (0, 0));
    assert_eq!(counts(&db, &both.proposal).await, before);
    assert_eq!(persisted_rows(&db, &both.proposal).await, rows_before);
    let stored: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT linked_spike_task_id,needs_evidence_claim FROM proposals WHERE id=$1",
    )
    .bind(&both.proposal)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(stored.0.as_deref(), Some(both.spike.as_str()));
    assert_eq!(
        serde_json::from_str::<Value>(&stored.1.unwrap()).unwrap(),
        c
    );
    assert_eq!(
        typed
            .dual_read_legacy_parity(&both.proposal)
            .await
            .unwrap()
            .unwrap()
            .finding
            .id,
        id
    );

    // Terminal linked state is inactive control: neither typed state nor legacy gating changes.
    let inactive = seed(&db).await;
    let c = claim(&inactive);
    legacy(&db, &inactive, Some(&inactive.spike), Some(&c)).await;
    sqlx::query("UPDATE tasks SET status='closed' WHERE id=$1")
        .bind(&inactive.spike)
        .execute(db.pool())
        .await
        .unwrap();
    typed.backfill_active_legacy_evidence().await.unwrap();
    assert_eq!(counts(&db, &inactive.proposal).await, (0, 0, 0));
    assert_eq!(
        typed
            .dual_read_legacy_parity(&inactive.proposal)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn typed_evidence_backfill_dual_write_clear_and_rollbacks() {
    let db = Database::ephemeral().await.unwrap();
    db.ensure_initialized().await.unwrap();
    let typed = TypedEvidenceRepository::new(db.clone());
    let s = seed(&db).await;
    let c = claim(&s);
    let demand = |finding_id| DemandTypedEvidenceInput {
        finding_id,
        proposal_id: s.proposal.clone(),
        demand_hash: legacy_demand_hash(&c, Some(&s.spike)),
        claim: c.clone(),
        demanded_revision_seq: 7,
        judge_task_id: s.creator.clone(),
    };

    // Production atomic set primitive commits matching legacy and typed authority.
    let mut tx = db.pool().begin().await.unwrap();
    let created = TypedEvidenceRepository::demand_activate_and_set_legacy_in_transaction(
        &mut tx,
        demand(uuid::Uuid::now_v7().to_string()),
        &s.spike,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(
        typed
            .dual_read_legacy_parity(&s.proposal)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(counts(&db, &s.proposal).await, (1, 1, 2));

    // Fail closed before clear: malformed parity leaves both representations unchanged.
    sqlx::query("UPDATE proposals SET needs_evidence_claim='{}' WHERE id=$1")
        .bind(&s.proposal)
        .execute(db.pool())
        .await
        .unwrap();
    let before = counts(&db, &s.proposal).await;
    let mut tx = db.pool().begin().await.unwrap();
    let err = TypedEvidenceRepository::transition_and_clear_legacy_in_transaction(
        &mut tx,
        &s.proposal,
        &s.spike,
        AppendTypedEvidenceTransitionInput {
            id: uuid::Uuid::now_v7().to_string(),
            finding_id: created.finding.id.clone(),
            ordinal: 3,
            from_lifecycle: Some(TribunalEvidenceLifecycle::SpikeActive),
            to_lifecycle: TribunalEvidenceLifecycle::EvidenceReceived,
            actor_task_id: Some(s.spike.clone()),
            metadata: json!({}),
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("legacy_typed_parity_mismatch"));
    tx.rollback().await.unwrap();
    assert_eq!(counts(&db, &s.proposal).await, before);

    // Inject failures after production primitive writes: aborting each caller-owned transaction leaves no partial typed or legacy residue.
    for _side in ["typed", "legacy"] {
        let isolated = seed(&db).await;
        let ci = claim(&isolated);
        let mut tx = db.pool().begin().await.unwrap();
        let input = DemandTypedEvidenceInput {
            finding_id: uuid::Uuid::now_v7().to_string(),
            proposal_id: isolated.proposal.clone(),
            demand_hash: legacy_demand_hash(&ci, Some(&isolated.spike)),
            claim: ci,
            demanded_revision_seq: 7,
            judge_task_id: isolated.creator.clone(),
        };
        TypedEvidenceRepository::demand_activate_and_set_legacy_in_transaction(
            &mut tx,
            input,
            &isolated.spike,
        )
        .await
        .unwrap();
        let injected: Result<(), &str> = Err("deterministic injected failure before commit");
        assert!(injected.is_err());
        tx.rollback().await.unwrap();
        assert_eq!(counts(&db, &isolated.proposal).await, (0, 0, 0));
        let legacy: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT linked_spike_task_id,needs_evidence_claim FROM proposals WHERE id=$1",
        )
        .bind(&isolated.proposal)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(legacy, (None, None));
    }
}
