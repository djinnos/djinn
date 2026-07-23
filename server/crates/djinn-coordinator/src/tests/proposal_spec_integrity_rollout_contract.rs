//! Static rollout contract for the proposal-integrity sweep.
//!
//! This deliberately validates repository artifacts only: it neither starts a
//! coordinator nor requires a database, Kubernetes, or production credentials.

use crate::context::ProposalSpecIntegritySweepConfig;
use crate::doctor::leader_tick::PROPOSAL_SPEC_INTEGRITY_CHECK_NAME;

const ROLLOUT_CHECKLIST: &str =
    include_str!("../../../../docs/operational/proposal-spec-integrity-sweep-rollout.md");
const SWEEP_CONFIG_SOURCE: &str = include_str!("../context.rs");
const SWEEP_SOURCE: &str = include_str!("../doctor/leader_tick.rs");
const LINT_MIGRATION: &str =
    include_str!("../../../djinn-db/migrations_postgres/137_proposal_revision_lint_results.sql");
const ACTIVE_FINDINGS_MIGRATION: &str =
    include_str!("../../../djinn-db/migrations_postgres/145_doctor_active_findings.sql");

const FLAG_NAME: &str = "proposal_spec_integrity_sweep_v1";
const FLAG_ENV: &str = "DJINN_PROPOSAL_SPEC_INTEGRITY_SWEEP_V1";
const ORDERED_STAGES: &[&str] = &[
    "**Shared linter and fixtures.**",
    "**Additive migrations.**",
    "**Repository invariant.**",
    "**API/DoR/tribunal consumers.**",
    "**UI.**",
    "**Disabled sweep deployment.**",
    "**Canary observation.**",
    "**Sweep enablement.**",
];

fn assert_in_order(document: &str, stages: &[&str]) {
    let mut after = 0;
    for stage in stages {
        let relative = document[after..]
            .find(stage)
            .unwrap_or_else(|| panic!("checklist is missing rollout stage: {stage}"));
        after += relative + stage.len();
    }
}

#[test]
fn proposal_integrity_rollout_contract_pins_disabled_flag_schema_and_order() {
    assert!(
        !ProposalSpecIntegritySweepConfig::default().enabled,
        "the retroactive-load flag must be initially disabled"
    );
    assert!(
        SWEEP_CONFIG_SOURCE.contains(FLAG_ENV),
        "the coordinator config must own the documented flag environment variable"
    );
    assert!(
        SWEEP_CONFIG_SOURCE.contains("unwrap_or(false)"),
        "an absent flag must remain disabled"
    );
    assert!(
        SWEEP_SOURCE.contains(PROPOSAL_SPEC_INTEGRITY_CHECK_NAME),
        "the sweep implementation must own the documented check name"
    );

    assert!(
        LINT_MIGRATION.contains("CREATE TABLE IF NOT EXISTS proposal_revision_lint_results"),
        "migration 137 must provide the additive immutable lint-result schema"
    );
    assert!(
        LINT_MIGRATION.contains("ADD COLUMN IF NOT EXISTS deduplication_key"),
        "migration 137 must provide the immutable finding deduplication key"
    );
    assert!(
        ACTIVE_FINDINGS_MIGRATION.contains("doctor_findings_active_key_idx"),
        "migration 145 remains the separate active-finding reconciliation schema"
    );

    assert_in_order(ROLLOUT_CHECKLIST, ORDERED_STAGES);
    for required in [
        FLAG_NAME,
        FLAG_ENV,
        PROPOSAL_SPEC_INTEGRITY_CHECK_NAME,
        "137_proposal_revision_lint_results.sql",
        "migration 145",
        "proposal_revision_lint_results",
        "doctor_findings.deduplication_key",
        "immediate retroactive-load kill\nswitch",
        "all immutable lint rows",
        "historical `proposal_spec_integrity_v1` doctor findings",
        "Do not delete proposal lint rows, doctor findings, proposal revisions, or\nproposal bodies",
        "neither Kubernetes,\nproduction credentials, a live canary",
    ] {
        assert!(
            ROLLOUT_CHECKLIST.contains(required),
            "checklist must name the rollout contract requirement: {required}"
        );
    }
}
