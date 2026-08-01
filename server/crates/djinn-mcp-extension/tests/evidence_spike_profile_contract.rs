//! Exact public tool-name contract for Judge and evidence-spike profiles.
//!
//! The fixture deliberately pins both profiles together: a grounded evidence
//! spike gains only the constrained plan/executor controls while retaining its
//! Architect readers, and it never inherits the Judge's ordinary shell tool.

use std::collections::BTreeSet;

use djinn_mcp_extension::tool_defs::{tool_schemas_evidence_spike, tool_schemas_judge};
use serde::Deserialize;

const FIXTURE: &str = include_str!("fixtures/evidence_spike_profile.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceSpikeProfileFixture {
    judge: BTreeSet<String>,
    evidence_spike: BTreeSet<String>,
}

fn tool_names(schemas: &[serde_json::Value]) -> BTreeSet<String> {
    schemas
        .iter()
        .map(|schema| {
            schema
                .get("name")
                .and_then(serde_json::Value::as_str)
                .expect("every advertised tool schema has a string name")
                .to_owned()
        })
        .collect()
}

#[test]
fn evidence_spike_profile_contract() {
    let fixture: EvidenceSpikeProfileFixture =
        serde_json::from_str(FIXTURE).expect("evidence spike profile fixture must be valid JSON");
    let judge = tool_names(&tool_schemas_judge());
    let spike = tool_names(&tool_schemas_evidence_spike());

    assert_eq!(judge, fixture.judge, "Judge tool names changed");
    assert_eq!(spike, fixture.evidence_spike, "evidence-spike tool names changed");

    assert!(judge.contains("shell"), "Judge retains ordinary shell");
    assert!(
        !judge.contains("code_graph") && !judge.contains("pr_review_context"),
        "Judge must not inherit Architect-only readers"
    );
    assert!(
        spike.contains("code_graph") && spike.contains("pr_review_context"),
        "evidence spike retains Architect investigation readers"
    );
    assert!(
        !spike.contains("shell"),
        "evidence spike must not expose ordinary shell"
    );
    assert!(
        spike.contains("evidence_plan") && spike.contains("evidence_exec"),
        "evidence spike must advertise both grounded evidence controls"
    );
    assert!(
        !judge.contains("evidence_plan") && !judge.contains("evidence_exec"),
        "Judge tool surface must remain unchanged by evidence controls"
    );
}
