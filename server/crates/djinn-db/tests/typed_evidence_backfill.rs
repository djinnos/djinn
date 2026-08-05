//! PostgreSQL integration coverage for mixed-version typed-evidence authority.
//! Assertions inspect persisted rows; fixture labels are never used as behavior.

use djinn_core::{events::EventBus, models::TribunalEvidenceLifecycle};
use djinn_db::{
    AppendTypedEvidenceTransitionInput, Database, DemandTypedEvidenceInput, ProposalRepository,
    TypedEvidenceRepository, legacy_demand_hash,
};
use serde_json::{Value, json};

struct Seed {
    proposal: String,
    creator: String,
    spike: String,
}
type Finding = (String, String, String, String, Value, i32, String);
type Attempt = (String, String, i32, String);
type Transition = (String, i32, Option<String>, String, Option<String>, Value);
type Snapshot = (
    (Option<String>, Option<String>),
    Vec<Finding>,
    Vec<Attempt>,
    Vec<Transition>,
);

async fn insert_task(db: &Database, project: &str, user: &str, title: &str) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,labels,acceptance_criteria,memory_refs,created_by_user_id) VALUES ($1,$2,$3,$4,'','','[]','[]','[]',$5)")
        .bind(&id).bind(project).bind(id.replace('-', "")).bind(title).bind(user).execute(db.pool()).await.unwrap();
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
    sqlx::query("INSERT INTO proposals (id,short_id,title,body,body_format,acceptance_criteria,status,latest_revision_seq) VALUES ($1,$2,'p','','markdown','[]','draft',7)").bind(&proposal).bind(proposal.replace('-',"")).execute(db.pool()).await.unwrap();
    Seed {
        proposal,
        creator,
        spike,
    }
}
fn claim(s: &Seed) -> Value {
    json!({"created_by_task_id":s.creator,"against_revision_seq":7,"uncertainty":"load-bearing"})
}
async fn set_legacy(db: &Database, s: &Seed, link: Option<&str>, c: Option<&Value>) {
    sqlx::query("UPDATE proposals SET linked_spike_task_id=$1,needs_evidence_claim=$2 WHERE id=$3")
        .bind(link)
        .bind(c.map(|x| serde_json::to_string(x).unwrap()))
        .bind(&s.proposal)
        .execute(db.pool())
        .await
        .unwrap();
}
async fn legacy(db: &Database, p: &str) -> (Option<String>, Option<String>) {
    sqlx::query_as("SELECT linked_spike_task_id,needs_evidence_claim FROM proposals WHERE id=$1")
        .bind(p)
        .fetch_one(db.pool())
        .await
        .unwrap()
}
async fn typed_rows(db: &Database, p: &str) -> (Vec<Finding>, Vec<Attempt>, Vec<Transition>) {
    let f=sqlx::query_as("SELECT id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id FROM typed_evidence_findings WHERE proposal_id=$1 ORDER BY id").bind(p).fetch_all(db.pool()).await.unwrap();
    let a=sqlx::query_as("SELECT id,finding_id,sequence,spike_task_id FROM typed_evidence_attempts WHERE finding_id IN (SELECT id FROM typed_evidence_findings WHERE proposal_id=$1) ORDER BY sequence,id").bind(p).fetch_all(db.pool()).await.unwrap();
    let t=sqlx::query_as("SELECT id,ordinal,from_lifecycle,to_lifecycle,actor_task_id,metadata FROM typed_evidence_transitions WHERE finding_id IN (SELECT id FROM typed_evidence_findings WHERE proposal_id=$1) ORDER BY ordinal,id").bind(p).fetch_all(db.pool()).await.unwrap();
    (f, a, t)
}
async fn snapshot(db: &Database, p: &str) -> Snapshot {
    let (f, a, t) = typed_rows(db, p).await;
    (legacy(db, p).await, f, a, t)
}
async fn assert_fail_closed(db: &Database, typed: &TypedEvidenceRepository, p: &str) {
    let before = snapshot(db, p).await;
    assert_eq!(typed.dual_read_legacy_parity(p).await.unwrap(), None);
    assert_eq!(
        snapshot(db, p).await,
        before,
        "fail-closed read mutated authority"
    );
}
async fn active(db: &Database, typed: &TypedEvidenceRepository) -> Seed {
    let s = seed(db).await;
    set_legacy(db, &s, Some(&s.spike), Some(&claim(&s))).await;
    typed.backfill_active_legacy_evidence().await.unwrap();
    s
}

#[tokio::test]
async fn typed_evidence_backfill_postgres_parity_matrix() {
    let db = Database::ephemeral().await.unwrap();
    db.ensure_initialized().await.unwrap();
    let typed = TypedEvidenceRepository::new(db.clone());
    let claim_only = seed(&db).await;
    let c = claim(&claim_only);
    set_legacy(&db, &claim_only, None, Some(&c)).await;
    let authority = legacy(&db, &claim_only.proposal).await;
    assert_eq!(
        typed
            .backfill_active_legacy_evidence()
            .await
            .unwrap()
            .created_findings,
        1
    );
    let (f, a, t) = typed_rows(&db, &claim_only.proposal).await;
    assert_eq!(f.len(), 1);
    assert!(a.is_empty());
    assert_eq!(t.len(), 1);
    assert_eq!(
        (&f[0].1, &f[0].2, &f[0].3, &f[0].4, f[0].5, &f[0].6),
        (
            &claim_only.proposal,
            &legacy_demand_hash(&c, None),
            &"demanded".to_owned(),
            &c,
            7,
            &claim_only.creator
        )
    );
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
    assert_eq!(legacy(&db, &claim_only.proposal).await, authority);
    let link_only = seed(&db).await;
    set_legacy(&db, &link_only, Some(&link_only.spike), None).await;
    let authority = legacy(&db, &link_only.proposal).await;
    typed.backfill_active_legacy_evidence().await.unwrap();
    let (f, a, t) = typed_rows(&db, &link_only.proposal).await;
    let synthetic = json!({"__typed_evidence_legacy_link_only":true});
    assert_eq!(
        (&f[0].1, &f[0].2, &f[0].3, &f[0].4, f[0].5, &f[0].6),
        (
            &link_only.proposal,
            &legacy_demand_hash(&synthetic, Some(&link_only.spike)),
            &"spike_active".to_owned(),
            &synthetic,
            7,
            &link_only.spike
        )
    );
    assert_eq!((&a[0].1, a[0].2, &a[0].3), (&f[0].0, 1, &link_only.spike));
    assert_eq!(
        t.iter()
            .map(|x| (x.1, x.2.clone(), x.3.clone(), x.4.clone(), x.5.clone()))
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
    assert_eq!(legacy(&db, &link_only.proposal).await, authority);
    let both = seed(&db).await;
    let c = claim(&both);
    set_legacy(&db, &both, Some(&both.spike), Some(&c)).await;
    let authority = legacy(&db, &both.proposal).await;
    typed.backfill_active_legacy_evidence().await.unwrap();
    let before = snapshot(&db, &both.proposal).await;
    let (f, a, t) = typed_rows(&db, &both.proposal).await;
    assert_eq!(
        (&f[0].1, &f[0].2, &f[0].3, &f[0].4, f[0].5, &f[0].6),
        (
            &both.proposal,
            &legacy_demand_hash(&c, Some(&both.spike)),
            &"spike_active".to_owned(),
            &c,
            7,
            &both.creator
        )
    );
    assert_eq!((&a[0].1, a[0].2, &a[0].3), (&f[0].0, 1, &both.spike));
    assert_eq!(
        t.iter()
            .map(|x| (x.1, x.2.clone(), x.3.clone(), x.4.clone(), x.5.clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                1,
                None,
                "demanded".into(),
                Some(both.creator.clone()),
                json!({"source":"legacy_backfill"})
            ),
            (
                2,
                Some("demanded".into()),
                "spike_active".into(),
                Some(both.spike.clone()),
                json!({"source":"legacy_backfill"})
            )
        ]
    );
    typed.backfill_active_legacy_evidence().await.unwrap();
    assert_eq!(snapshot(&db, &both.proposal).await, before);
    assert_eq!(legacy(&db, &both.proposal).await, authority);
    let inactive = seed(&db).await;
    let c = claim(&inactive);
    set_legacy(&db, &inactive, Some(&inactive.spike), Some(&c)).await;
    sqlx::query("UPDATE tasks SET status='closed' WHERE id=$1")
        .bind(&inactive.spike)
        .execute(db.pool())
        .await
        .unwrap();
    let before = snapshot(&db, &inactive.proposal).await;
    typed.backfill_active_legacy_evidence().await.unwrap();
    assert_eq!(
        typed_rows(&db, &inactive.proposal).await,
        (vec![], vec![], vec![])
    );
    assert_eq!(
        typed
            .dual_read_legacy_parity(&inactive.proposal)
            .await
            .unwrap(),
        None
    );
    assert_eq!(snapshot(&db, &inactive.proposal).await, before);
}

#[tokio::test]
async fn typed_evidence_backfill_fail_closed_mismatch_matrix() {
    let db = Database::ephemeral().await.unwrap();
    db.ensure_initialized().await.unwrap();
    let typed = TypedEvidenceRepository::new(db.clone());
    let malformed = seed(&db).await;
    set_legacy(&db, &malformed, Some(&malformed.spike), None).await;
    sqlx::query("UPDATE proposals SET needs_evidence_claim='{}' WHERE id=$1")
        .bind(&malformed.proposal)
        .execute(db.pool())
        .await
        .unwrap();
    assert_fail_closed(&db, &typed, &malformed.proposal).await;
    let missing = active(&db, &typed).await;
    // Test setup deliberately creates missing authority; bypass only the
    // append-only guards while constructing that corrupt persisted fixture.
    sqlx::query("ALTER TABLE typed_evidence_transitions DISABLE TRIGGER typed_evidence_transitions_append_only")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "ALTER TABLE typed_evidence_attempts DISABLE TRIGGER typed_evidence_attempts_append_only",
    )
    .execute(db.pool())
    .await
    .unwrap();
    for q in [
        "DELETE FROM typed_evidence_transitions WHERE finding_id IN (SELECT id FROM typed_evidence_findings WHERE proposal_id=$1)",
        "DELETE FROM typed_evidence_attempts WHERE finding_id IN (SELECT id FROM typed_evidence_findings WHERE proposal_id=$1)",
        "DELETE FROM typed_evidence_findings WHERE proposal_id=$1",
    ] {
        sqlx::query(q)
            .bind(&missing.proposal)
            .execute(db.pool())
            .await
            .unwrap();
    }
    sqlx::query("ALTER TABLE typed_evidence_transitions ENABLE TRIGGER typed_evidence_transitions_append_only")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "ALTER TABLE typed_evidence_attempts ENABLE TRIGGER typed_evidence_attempts_append_only",
    )
    .execute(db.pool())
    .await
    .unwrap();
    assert_fail_closed(&db, &typed, &missing.proposal).await;
    let ambiguous = active(&db, &typed).await;
    sqlx::query("DROP INDEX typed_evidence_one_unresolved_finding_per_proposal")
        .execute(db.pool())
        .await
        .unwrap();
    let extra_finding = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,'demanded',$4,7,$5)").bind(&extra_finding).bind(&ambiguous.proposal).bind(format!("other-{}",uuid::Uuid::now_v7())).bind(claim(&ambiguous)).bind(&ambiguous.creator).execute(db.pool()).await.unwrap();
    assert_fail_closed(&db, &typed, &ambiguous.proposal).await;
    sqlx::query("DELETE FROM typed_evidence_findings WHERE id=$1")
        .bind(extra_finding)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("CREATE UNIQUE INDEX typed_evidence_one_unresolved_finding_per_proposal ON typed_evidence_findings(proposal_id) WHERE lifecycle IN ('demanded','spike_active','evidence_received','failed')")
        .execute(db.pool())
        .await
        .unwrap();
    let task_state = active(&db, &typed).await;
    sqlx::query("UPDATE tasks SET status='closed' WHERE id=$1")
        .bind(&task_state.spike)
        .execute(db.pool())
        .await
        .unwrap();
    assert_fail_closed(&db, &typed, &task_state.proposal).await;
    for (column, value) in [
        ("claim", "'{\"different\":true}'::jsonb"),
        ("demand_hash", "'wrong'"),
        ("lifecycle", "'demanded'"),
    ] {
        let s = active(&db, &typed).await;
        sqlx::query(&format!(
            "UPDATE typed_evidence_findings SET {column}={value} WHERE proposal_id=$1"
        ))
        .bind(&s.proposal)
        .execute(db.pool())
        .await
        .unwrap();
        assert_fail_closed(&db, &typed, &s.proposal).await;
    }
}

#[tokio::test]
async fn typed_evidence_backfill_dual_write_clear_rollback_and_reverse_rollback() {
    let db = Database::ephemeral().await.unwrap();
    db.ensure_initialized().await.unwrap();
    let typed = TypedEvidenceRepository::new(db.clone());
    let s = seed(&db).await;
    let c = claim(&s);
    let demand = |id| DemandTypedEvidenceInput {
        finding_id: id,
        proposal_id: s.proposal.clone(),
        demand_hash: legacy_demand_hash(&c, Some(&s.spike)),
        claim: c.clone(),
        demanded_revision_seq: 7,
        judge_task_id: s.creator.clone(),
    };
    let mut tx = db.pool().begin().await.unwrap();
    TypedEvidenceRepository::demand_activate_and_set_legacy_in_transaction(
        &mut tx,
        demand(uuid::Uuid::now_v7().to_string()),
        &s.spike,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (f, a, t) = typed_rows(&db, &s.proposal).await;
    let stored_legacy = legacy(&db, &s.proposal).await;
    assert_eq!(stored_legacy.0.as_deref(), Some(s.spike.as_str()));
    assert_eq!(
        serde_json::from_str::<Value>(stored_legacy.1.as_deref().unwrap()).unwrap(),
        c
    );
    assert_eq!(
        (&f[0].1, &f[0].2, f[0].3.as_str(), &f[0].4, f[0].5, &f[0].6),
        (
            &s.proposal,
            &legacy_demand_hash(&c, Some(&s.spike)),
            "spike_active",
            &c,
            7,
            &s.creator
        )
    );
    assert_eq!((&a[0].1, a[0].2, &a[0].3), (&f[0].0, 1, &s.spike));
    assert_eq!(
        t.iter()
            .map(|row| (row.1, row.2.as_deref(), row.3.as_str(), row.4.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            (1, None, "demanded", Some(s.creator.as_str())),
            (2, Some("demanded"), "spike_active", Some(s.spike.as_str()))
        ]
    );
    sqlx::query("UPDATE tasks SET status='closed' WHERE id=$1")
        .bind(&s.spike)
        .execute(db.pool())
        .await
        .unwrap();
    typed
        .evidence_received_and_clear_legacy(&s.proposal, &s.spike)
        .await
        .unwrap();
    let (f, a, t) = typed_rows(&db, &s.proposal).await;
    assert_eq!(legacy(&db, &s.proposal).await, (None, None));
    assert_eq!(f[0].3, "evidence_received");
    assert_eq!(
        (
            t[2].1,
            t[2].2.as_deref(),
            t[2].3.as_str(),
            t[2].4.as_deref(),
            &t[2].5
        ),
        (
            3,
            Some("spike_active"),
            "evidence_received",
            Some(s.spike.as_str()),
            &json!({"source":"legacy_dual_write_clear","attempt_id":a[0].0.clone()})
        )
    );
    // The production set API writes the finding and legacy columns before it
    // allocates the active attempt. Reject that insert at the real boundary.
    let set_failure = seed(&db).await;
    let set_claim = claim(&set_failure);
    let before = snapshot(&db, &set_failure.proposal).await;
    sqlx::query("CREATE FUNCTION typed_evidence_test_reject_attempt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'typed evidence test attempt rejection'; END $$").execute(db.pool()).await.unwrap();
    sqlx::query("CREATE TRIGGER typed_evidence_test_reject_attempt BEFORE INSERT ON typed_evidence_attempts FOR EACH ROW EXECUTE FUNCTION typed_evidence_test_reject_attempt()").execute(db.pool()).await.unwrap();
    let mut tx = db.pool().begin().await.unwrap();
    let result = TypedEvidenceRepository::demand_activate_and_set_legacy_in_transaction(
        &mut tx,
        DemandTypedEvidenceInput {
            finding_id: uuid::Uuid::now_v7().to_string(),
            proposal_id: set_failure.proposal.clone(),
            demand_hash: legacy_demand_hash(&set_claim, Some(&set_failure.spike)),
            claim: set_claim,
            demanded_revision_seq: 7,
            judge_task_id: set_failure.creator.clone(),
        },
        &set_failure.spike,
    )
    .await;
    assert!(result.is_err());
    drop(tx);
    sqlx::query("DROP TRIGGER typed_evidence_test_reject_attempt ON typed_evidence_attempts")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION typed_evidence_test_reject_attempt()")
        .execute(db.pool())
        .await
        .unwrap();
    let after = snapshot(&db, &set_failure.proposal).await;
    assert_eq!(after.0, before.0, "legacy set was not rolled back");
    assert_eq!(after.1, before.1, "finding set was not rolled back");
    assert_eq!(after.2, before.2, "attempt set was not rolled back");
    assert_eq!(after.3, before.3, "transition set was not rolled back");

    // Begin populated, append the typed clear transition, then reject only the
    // legacy UPDATE which clears non-null compatibility authority.
    let clear_failure = active(&db, &typed).await;
    let before = snapshot(&db, &clear_failure.proposal).await;
    let finding_id = before.1[0].0.clone();
    let attempt_id = before.2[0].0.clone();
    sqlx::query("CREATE FUNCTION typed_evidence_test_reject_legacy_clear() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF OLD.linked_spike_task_id IS NOT NULL AND OLD.needs_evidence_claim IS NOT NULL AND NEW.linked_spike_task_id IS NULL AND NEW.needs_evidence_claim IS NULL THEN RAISE EXCEPTION 'typed evidence test legacy clear rejection'; END IF; RETURN NEW; END $$").execute(db.pool()).await.unwrap();
    sqlx::query("CREATE TRIGGER typed_evidence_test_reject_legacy_clear BEFORE UPDATE ON proposals FOR EACH ROW EXECUTE FUNCTION typed_evidence_test_reject_legacy_clear()").execute(db.pool()).await.unwrap();
    let mut tx = db.pool().begin().await.unwrap();
    let result = TypedEvidenceRepository::transition_and_clear_legacy_in_transaction(
        &mut tx,
        &clear_failure.proposal,
        &clear_failure.spike,
        AppendTypedEvidenceTransitionInput {
            id: uuid::Uuid::now_v7().to_string(),
            finding_id,
            ordinal: 3,
            from_lifecycle: Some(TribunalEvidenceLifecycle::SpikeActive),
            to_lifecycle: TribunalEvidenceLifecycle::EvidenceReceived,
            actor_task_id: Some(clear_failure.spike.clone()),
            metadata: json!({"source":"legacy_dual_write_clear","attempt_id":attempt_id}),
        },
    )
    .await;
    assert!(result.is_err());
    drop(tx);
    sqlx::query("DROP TRIGGER typed_evidence_test_reject_legacy_clear ON proposals")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION typed_evidence_test_reject_legacy_clear()")
        .execute(db.pool())
        .await
        .unwrap();
    let after = snapshot(&db, &clear_failure.proposal).await;
    assert_eq!(after.0, before.0, "legacy columns were cleared");
    assert!(after.0.0.is_some() && after.0.1.is_some());
    assert_eq!(after.1, before.1, "finding transition was not rolled back");
    assert_eq!(after.2, before.2, "attempt rows changed during clear");
    assert_eq!(after.3, before.3, "transition append was not rolled back");
    let rollback = active(&db, &typed).await;
    let history = typed_rows(&db, &rollback.proposal).await;
    let legacy_repo = ProposalRepository::new(db.clone(), EventBus::noop());
    assert_eq!(
        legacy_repo
            .find_by_linked_spike(&rollback.spike)
            .await
            .unwrap()
            .unwrap()
            .id,
        rollback.proposal
    );
    assert_eq!(typed_rows(&db, &rollback.proposal).await, history);
    for c in legacy_repo
        .list_linked_evidence_spike_recovery_candidates()
        .await
        .unwrap()
        .into_iter()
        .filter(|x| x.linked_spike_task_status != "closed")
    {
        let n:i64=sqlx::query_scalar("SELECT count(*) FROM typed_evidence_attempts a JOIN typed_evidence_findings f ON f.id=a.finding_id WHERE f.proposal_id=$1 AND f.lifecycle='spike_active' AND a.spike_task_id=$2").bind(&c.proposal_id).bind(&c.linked_spike_task_id).fetch_one(db.pool()).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            legacy(&db, &c.proposal_id).await.0.as_deref(),
            Some(c.linked_spike_task_id.as_str())
        );
    }
    assert_eq!(TribunalEvidenceLifecycle::Demanded.as_str(), "demanded");
}
