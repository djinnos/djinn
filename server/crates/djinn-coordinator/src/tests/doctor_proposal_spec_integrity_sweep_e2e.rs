//! Focused Postgres-backed smoke coverage for proposal integrity sweep.

use djinn_core::events::EventBus;
use djinn_db::repositories::proposal::ProposalCreateInput;
use djinn_db::{DoctorFindingRepository, ProposalIntegrityHeadPage, ProposalIntegrityRepository, ProposalRepository, RecentDoctorFindings};

use crate::doctor::leader_tick::{run_proposal_spec_integrity_sweep, PROPOSAL_SPEC_INTEGRITY_CHECK_NAME};
use crate::test_helpers;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_integrity_sweep_pages_active_heads_and_is_idempotent() {
    let db = test_helpers::create_test_db();
    let proposals = ProposalRepository::new(db.clone(), EventBus::noop());
    let mut active = Vec::new();
    for status in ["draft", "in_review", "approved", "building"] {
        active.push(proposals.create(ProposalCreateInput { title: "fixture", body: "# Goal\n", acceptance_criteria: Some("[\"works\"]"), status: Some(status), body_format: None }).await.expect("create").id);
    }
    for status in ["triage", "done", "rejected", "archived", "superseded"] {
        proposals.create(ProposalCreateInput { title: "terminal", body: "# Goal\n", acceptance_criteria: Some("[\"works\"]"), status: Some(status), body_format: None }).await.expect("create");
    }
    let heads = ProposalIntegrityRepository::new(db.clone());
    let first = heads.list_current_heads(ProposalIntegrityHeadPage { after_proposal_id: None, limit: 2 }).await.expect("first page");
    let second = heads.list_current_heads(ProposalIntegrityHeadPage { after_proposal_id: Some(first[1].proposal_id.clone()), limit: 2 }).await.expect("second page");
    assert_eq!(first.len() + second.len(), active.len());
    assert!(first.iter().chain(second.iter()).collect::<Vec<_>>().windows(2).all(|p| p[0].proposal_id < p[1].proposal_id));
    run_proposal_spec_integrity_sweep(true, &db, Some("first")).await;
    run_proposal_spec_integrity_sweep(true, &db, Some("rerun")).await;
    let findings = DoctorFindingRepository::new(db).list_recent(RecentDoctorFindings { check_name: Some(PROPOSAL_SPEC_INTEGRITY_CHECK_NAME.to_owned()), limit: Some(20), ..Default::default() }).await.expect("findings");
    assert_eq!(findings.len(), active.len());
    for finding in findings {
        assert!(active.contains(&finding.entity_ids[0].as_str().expect("proposal id").to_owned()));
        assert_eq!(finding.evidence["revision_seq"], 1);
        assert_eq!(finding.evidence["linter_version"], "v1");
        assert_eq!(finding.evidence["body_sha256"].as_str().map(str::len), Some(64));
    }
}
