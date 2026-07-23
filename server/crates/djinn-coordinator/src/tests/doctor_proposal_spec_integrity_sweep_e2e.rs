//! Focused Postgres-backed invariant coverage for the proposal integrity sweep.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_core::models::ProposalRevision;
use djinn_db::repositories::proposal::{ProposalCreateInput, ProposalUpdateInput};
use djinn_db::{
    DoctorFindingRepository, LintMaterializationOutcome, NewDoctorFinding, ProposalIntegrityHead,
    ProposalIntegrityHeadPage, ProposalIntegrityRepository, ProposalRepository,
    RecentDoctorFindings,
};

use crate::doctor::leader_tick::{
    PROPOSAL_SPEC_INTEGRITY_CHECK_NAME, ProposalIntegritySweepSource,
    run_proposal_spec_integrity_sweep_with_source,
};
use crate::test_helpers;

const INVALID_BODY: &str = "before\n<Callout id=\"broken\">";
const CLEAN_BODY: &str = "# Goal\n\nA concrete retained implementation goal.\n";

/// Counting/fault adapter over real Postgres repositories. It only injects at
/// the production runner's source and snapshot-before-persist boundaries.
struct SweepSource {
    heads: ProposalIntegrityRepository,
    proposals: ProposalRepository,
    findings: DoctorFindingRepository,
    db: djinn_db::Database,
    scans: AtomicUsize,
    loads: AtomicUsize,
    fail_lint_once: Mutex<Option<String>>,
    advance_before_persist_once: Mutex<Option<String>>,
}

fn source(db: djinn_db::Database) -> SweepSource {
    SweepSource {
        heads: ProposalIntegrityRepository::new(db.clone()),
        proposals: ProposalRepository::new(db.clone(), EventBus::noop()),
        findings: DoctorFindingRepository::new(db.clone()),
        db,
        scans: AtomicUsize::new(0),
        loads: AtomicUsize::new(0),
        fail_lint_once: Mutex::new(None),
        advance_before_persist_once: Mutex::new(None),
    }
}

#[async_trait]
impl ProposalIntegritySweepSource for SweepSource {
    fn page_size(&self) -> i64 {
        2
    }

    async fn list_heads(
        &self,
        page: ProposalIntegrityHeadPage,
    ) -> djinn_db::Result<Vec<ProposalIntegrityHead>> {
        self.scans.fetch_add(1, Ordering::SeqCst);
        self.heads.list_current_heads(page).await
    }

    async fn lint(
        &self,
        revision: &ProposalRevision,
    ) -> djinn_db::Result<djinn_spec_lint::SpecLintResultV1> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        let fail = {
            let mut guard = self.fail_lint_once.lock().expect("failure guard");
            if guard.as_deref() == Some(&revision.proposal_id) {
                guard.take();
                true
            } else {
                false
            }
        };
        if fail {
            return Err(djinn_db::Error::InvalidData(
                "simulated interrupted lint".to_owned(),
            ));
        }
        self.proposals.lint_for_revision(revision).await
    }

    async fn before_persist(&self, head: &ProposalIntegrityHead) -> djinn_db::Result<()> {
        let advance = {
            let mut guard = self
                .advance_before_persist_once
                .lock()
                .expect("advance guard");
            if guard.as_deref() == Some(&head.proposal_id) {
                guard.take()
            } else {
                None
            }
        };
        if let Some(id) = advance {
            self.proposals
                .update(
                    &id,
                    ProposalUpdateInput {
                        title: "race fixture",
                        body: CLEAN_BODY,
                        acceptance_criteria: "[\"works\"]",
                        status: "draft",
                        superseded_by: None,
                        body_format: Some("mdx"),
                        event_metadata: None,
                    },
                )
                .await?;
            // A legacy head may predate the write-time lint gate. Replace the
            // newly-created clean revision with invalid retained text so the
            // next sweep must process a current, lint-failing revision.
            djinn_db::test_support::replace_legacy_proposal_head_for_test(
                &self.db,
                &id,
                INVALID_BODY,
                "mdx",
            )
            .await;
            djinn_db::test_support::delete_proposal_lint_results_for_test(&self.db, &id).await;
        }
        Ok(())
    }

    async fn materialize(
        &self,
        head: &ProposalIntegrityHead,
        result: &djinn_spec_lint::SpecLintResultV1,
    ) -> djinn_db::Result<LintMaterializationOutcome> {
        self.heads.materialize_if_current(head, result).await
    }

    async fn insert_finding(&self, finding: NewDoctorFinding, key: &str) -> djinn_db::Result<()> {
        self.findings
            .insert_ignore_duplicate(finding, key)
            .await
            .map(|_| ())
    }
}

async fn create(
    db: &djinn_db::Database,
    proposals: &ProposalRepository,
    title: &str,
    status: &str,
    body: &str,
) -> String {
    let proposal = proposals
        .create(ProposalCreateInput {
            title,
            // Repository writes reject malformed specifications. Seed a
            // valid revision first, then use the established legacy-data test
            // seam below when this fixture needs an invalid historical head.
            body: CLEAN_BODY,
            acceptance_criteria: Some("[\"works\"]"),
            status: Some(status),
            body_format: Some("mdx"),
        })
        .await
        .expect("create fixture");
    if body == INVALID_BODY {
        // The production sweep is retroactive and must support heads written
        // before lint enforcement, so use real Postgres legacy fixture data.
        djinn_db::test_support::replace_legacy_proposal_head_for_test(
            db,
            &proposal.id,
            body,
            "mdx",
        )
        .await;
        djinn_db::test_support::delete_proposal_lint_results_for_test(db, &proposal.id).await;
    }
    proposal.id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_integrity_sweep_proves_disabled_paging_resume_and_payload_invariants() {
    let db = test_helpers::create_test_db();
    let proposals = ProposalRepository::new(db.clone(), EventBus::noop());
    let mut active = Vec::new();
    for status in ["draft", "in_review", "approved", "building"] {
        active.push(create(&db, &proposals, status, status, INVALID_BODY).await);
    }
    active.sort();
    let mut terminal = Vec::new();
    for status in ["triage", "done", "rejected", "archived", "superseded"] {
        terminal.push(create(&db, &proposals, status, status, INVALID_BODY).await);
    }

    let heads = ProposalIntegrityRepository::new(db.clone());
    let mut cursor = None;
    let mut paged = Vec::new();
    loop {
        let page = heads
            .list_current_heads(ProposalIntegrityHeadPage {
                after_proposal_id: cursor,
                limit: 2,
            })
            .await
            .expect("page");
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 2, "deliberately bounded pages");
        cursor = page.last().map(|head| head.proposal_id.clone());
        paged.extend(page.into_iter().map(|head| head.proposal_id));
    }
    assert_eq!(
        paged, active,
        "exactly draft/in_review/approved/building heads paginate"
    );
    assert!(
        terminal.iter().all(|id| !paged.contains(id)),
        "all terminal statuses are excluded"
    );

    let source = source(db.clone());
    run_proposal_spec_integrity_sweep_with_source(false, &source, Some("disabled")).await;
    assert_eq!(
        source.scans.load(Ordering::SeqCst),
        0,
        "disabled does not scan"
    );
    assert_eq!(
        source.loads.load(Ordering::SeqCst),
        0,
        "disabled does not load bodies"
    );

    *source.fail_lint_once.lock().expect("failure guard") = Some(active[2].clone());
    run_proposal_spec_integrity_sweep_with_source(true, &source, Some("interrupted")).await;
    let findings = DoctorFindingRepository::new(db.clone());
    assert_eq!(
        findings
            .count_for_check(PROPOSAL_SPEC_INTEGRITY_CHECK_NAME)
            .await
            .expect("count"),
        3
    );
    run_proposal_spec_integrity_sweep_with_source(true, &source, Some("resume")).await;
    assert_eq!(
        findings
            .count_for_check(PROPOSAL_SPEC_INTEGRITY_CHECK_NAME)
            .await
            .expect("count"),
        4
    );

    let rows = findings
        .list_recent(RecentDoctorFindings {
            check_name: Some(PROPOSAL_SPEC_INTEGRITY_CHECK_NAME.to_owned()),
            limit: Some(10),
            ..Default::default()
        })
        .await
        .expect("findings");
    assert_eq!(
        rows.len(),
        4,
        "reruns preserve one immutable finding per proposal/revision/version"
    );
    for row in rows {
        let id = row.entity_ids[0].as_str().expect("proposal entity");
        assert!(active.contains(&id.to_owned()));
        assert_eq!(row.evidence["revision_seq"], 1);
        assert_eq!(
            row.evidence["body_sha256"],
            djinn_spec_lint::body_sha256(INVALID_BODY)
        );
        assert_eq!(row.evidence["linter_version"], "v1");
        assert_eq!(
            findings
                .deduplication_key(&row.id)
                .await
                .expect("finding key"),
            Some(format!("{PROPOSAL_SPEC_INTEGRITY_CHECK_NAME}:{id}:1:v1")),
            "persisted immutable finding key binds proposal/revision/version"
        );
        let head = heads
            .list_current_heads(ProposalIntegrityHeadPage {
                after_proposal_id: None,
                limit: 100,
            })
            .await
            .expect("heads")
            .into_iter()
            .find(|head| head.proposal_id == id)
            .expect("head");
        let expected = proposals
            .lint_for_revision(&head.revision)
            .await
            .expect("lint");
        assert_eq!(
            row.evidence["violations"],
            serde_json::to_value(expected.errors).expect("ordered violations")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_integrity_sweep_discards_stale_snapshot_then_keeps_historical_finding() {
    let db = test_helpers::create_test_db();
    let proposals = ProposalRepository::new(db.clone(), EventBus::noop());
    let id = create(&db, &proposals, "race fixture", "draft", INVALID_BODY).await;
    djinn_db::test_support::delete_proposal_lint_results_for_test(&db, &id).await;
    let source = source(db.clone());
    *source
        .advance_before_persist_once
        .lock()
        .expect("advance guard") = Some(id.clone());

    run_proposal_spec_integrity_sweep_with_source(true, &source, Some("stale")).await;
    assert_eq!(
        djinn_db::test_support::proposal_lint_revision_id_for_test(&db, &id, 1).await,
        None,
        "stale lint row is not published"
    );
    let findings = DoctorFindingRepository::new(db.clone());
    assert_eq!(
        findings
            .count_for_check(PROPOSAL_SPEC_INTEGRITY_CHECK_NAME)
            .await
            .expect("count"),
        0,
        "stale finding is not published"
    );

    run_proposal_spec_integrity_sweep_with_source(true, &source, Some("current")).await;
    assert!(
        djinn_db::test_support::proposal_lint_revision_id_for_test(&db, &id, 2)
            .await
            .is_some(),
        "current head is materialized"
    );
    assert_eq!(
        findings
            .count_for_check(PROPOSAL_SPEC_INTEGRITY_CHECK_NAME)
            .await
            .expect("count"),
        1
    );

    proposals
        .update(
            &id,
            ProposalUpdateInput {
                title: "race fixture",
                body: CLEAN_BODY,
                acceptance_criteria: "[\"works\"]",
                status: "draft",
                superseded_by: None,
                body_format: Some("markdown"),
                event_metadata: None,
            },
        )
        .await
        .expect("clean revision");
    run_proposal_spec_integrity_sweep_with_source(true, &source, Some("clean")).await;
    assert!(
        djinn_db::test_support::proposal_lint_revision_id_for_test(&db, &id, 3)
            .await
            .is_some(),
        "clean current head is materialized"
    );
    let rows = findings
        .list_recent(RecentDoctorFindings {
            check_name: Some(PROPOSAL_SPEC_INTEGRITY_CHECK_NAME.to_owned()),
            limit: Some(10),
            ..Default::default()
        })
        .await
        .expect("finding history");
    assert_eq!(
        rows.len(),
        1,
        "historical bad-revision finding remains and clean head creates none"
    );
    assert_eq!(rows[0].evidence["revision_seq"], 2);
}
