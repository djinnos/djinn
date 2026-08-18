//! Additive compatibility contract for typed refinement-evidence MCP tools.
//!
//! This fixture deliberately protects the legacy demand fields and response
//! shape while separately pinning typed finding/attempt/lifecycle/conflict,
//! retry, and disposition additions. It is not a general question inventory:
//! a demand carries one explicit claim plus durable typed identities.

use std::collections::BTreeSet;

use djinn_control_plane::tools::{
    proposal_ops::{
        EvidenceDispositionResponse, EvidenceRetryResponse, NeedsEvidenceDemandResponse,
    },
    refinement_helpers::ProposalRefinementDemandEvidenceParams,
    refinement_tools::{
        ProposalRefinementResolveEvidenceParams, ProposalRefinementRetryEvidenceParams,
        ProposalRefinementWithdrawEvidenceParams,
    },
};
use djinn_mcp_extension::{shared_schemas, tool_defs};
use serde::Deserialize;
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/typed_evidence_legacy_contract.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    legacy_demand_required: BTreeSet<String>,
    typed_demand_required: BTreeSet<String>,
    legacy_demand_response_fields: BTreeSet<String>,
    legacy_demand_result_fields: BTreeSet<String>,
    typed_demand_result_fields: BTreeSet<String>,
    typed_retry_response_fields: BTreeSet<String>,
    typed_disposition_response_fields: BTreeSet<String>,
    /// The **complete** input-property set of each evidence tool, by tool name.
    ///
    /// This is the checkable form of "the demand schema exposes no free-text
    /// question collection". The three `assert!(!properties.contains(...))`
    /// lines that stood here named `open_questions`, `question_form` and
    /// `prose` — none of which has ever been a field in any schema in this
    /// repository, so they could not fail. A closed set can: adding ANY
    /// property to any of the four tools reddens this test, whether or not
    /// whoever added it thought to name it something a blocklist anticipated.
    evidence_tool_input_properties: std::collections::BTreeMap<String, BTreeSet<String>>,
    demand_example: Value,
}

fn keys(value: &Value, path: &[&str]) -> BTreeSet<String> {
    let mut current = value;
    for segment in path {
        current = &current[*segment];
    }
    current
        .as_object()
        .unwrap_or_else(|| panic!("{} must be an object", path.join(".")))
        .keys()
        .cloned()
        .collect()
}

fn required(value: &Value, path: &[&str]) -> BTreeSet<String> {
    let mut current = value;
    for segment in path {
        current = &current[*segment];
    }
    current
        .as_array()
        .unwrap_or_else(|| panic!("{} must be an array", path.join(".")))
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .expect("required entry must be a string")
                .to_owned()
        })
        .collect()
}

fn fields<T: schemars::JsonSchema>() -> BTreeSet<String> {
    let schema = serde_json::to_value(schemars::schema_for!(T)).expect("serialize schema");
    keys(&schema, &["properties"])
}

fn has_all(actual: &BTreeSet<String>, expected: &BTreeSet<String>) -> Result<(), BTreeSet<String>> {
    let missing: BTreeSet<String> = expected.difference(actual).cloned().collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

#[test]
fn typed_evidence_legacy_contract() {
    let fixture: Fixture = serde_json::from_str(FIXTURE).expect("typed evidence fixture is valid");

    let demand = serde_json::to_value(shared_schemas::tool_proposal_refinement_demand_evidence())
        .expect("serialize demand MCP schema");
    let demand_required = required(&demand, &["inputSchema", "required"]);
    let demand_properties = keys(&demand, &["inputSchema", "properties"]);
    has_all(&demand_required, &fixture.legacy_demand_required)
        .expect("every legacy demand field remains required");
    has_all(&demand_properties, &fixture.legacy_demand_required)
        .expect("every legacy demand field remains accepted");
    has_all(&demand_required, &fixture.typed_demand_required)
        .expect("typed demand additions remain required");

    let handler_demand_fields = fields::<ProposalRefinementDemandEvidenceParams>();
    assert_eq!(
        demand_properties, handler_demand_fields,
        "advertised demand schema must exactly match handler parameters"
    );
    serde_json::from_value::<ProposalRefinementDemandEvidenceParams>(
        fixture.demand_example.clone(),
    )
    .expect("complete legacy demand payload plus typed category remains accepted");
    for legacy in &fixture.legacy_demand_required {
        let mut missing = fixture.demand_example.clone();
        missing
            .as_object_mut()
            .expect("fixture example is object")
            .remove(legacy);
        assert!(
            serde_json::from_value::<ProposalRefinementDemandEvidenceParams>(missing).is_err(),
            "removing legacy demand field `{legacy}` must fail deserialization"
        );
    }

    let demand_response = fields::<NeedsEvidenceDemandResponse>();
    has_all(&demand_response, &fixture.legacy_demand_response_fields)
        .expect("legacy demand top-level response fields remain represented");
    has_all(
        &demand_response,
        &BTreeSet::from(["conflict_code".to_owned()]),
    )
    .expect("typed conflict code is additive");
    let demand_result = keys(
        &serde_json::to_value(schemars::schema_for!(NeedsEvidenceDemandResponse))
            .expect("serialize demand response schema"),
        &["$defs", "NeedsEvidenceDemandResult", "properties"],
    );
    has_all(&demand_result, &fixture.legacy_demand_result_fields)
        .expect("legacy demand result fields remain represented");
    has_all(&demand_result, &fixture.typed_demand_result_fields)
        .expect("typed finding, attempt, lifecycle, and replay fields are additive");

    assert_eq!(
        fields::<EvidenceRetryResponse>(),
        fixture.typed_retry_response_fields
    );
    assert_eq!(
        fields::<EvidenceDispositionResponse>(),
        fixture.typed_disposition_response_fields
    );

    let retry = serde_json::to_value(shared_schemas::tool_proposal_refinement_retry_evidence())
        .expect("serialize retry MCP schema");
    assert_eq!(
        keys(&retry, &["inputSchema", "properties"]),
        fields::<ProposalRefinementRetryEvidenceParams>()
    );
    assert_eq!(
        required(&retry, &["inputSchema", "required"]),
        required(
            &serde_json::to_value(schemars::schema_for!(ProposalRefinementRetryEvidenceParams))
                .expect("serialize retry handler schema"),
            &["required"],
        )
    );
    let resolve = serde_json::to_value(shared_schemas::tool_proposal_refinement_resolve_evidence())
        .expect("serialize resolve MCP schema");
    assert_eq!(
        keys(&resolve, &["inputSchema", "properties"]),
        fields::<ProposalRefinementResolveEvidenceParams>()
    );
    assert_eq!(
        required(&resolve, &["inputSchema", "required"]),
        required(
            &serde_json::to_value(schemars::schema_for!(
                ProposalRefinementResolveEvidenceParams
            ))
            .expect("serialize resolve handler schema"),
            &["required"],
        )
    );
    let withdraw =
        serde_json::to_value(shared_schemas::tool_proposal_refinement_withdraw_evidence())
            .expect("serialize withdraw MCP schema");
    assert_eq!(
        keys(&withdraw, &["inputSchema", "properties"]),
        fields::<ProposalRefinementWithdrawEvidenceParams>()
    );

    assert_eq!(
        required(&withdraw, &["inputSchema", "required"]),
        required(
            &serde_json::to_value(schemars::schema_for!(
                ProposalRefinementWithdrawEvidenceParams
            ))
            .expect("serialize withdraw handler schema"),
            &["required"],
        )
    );

    let roles = [
        ("advocate", tool_defs::tool_schemas_advocate()),
        ("adversary", tool_defs::tool_schemas_adversary()),
        ("judge", tool_defs::tool_schemas_judge()),
    ];
    for (role, schemas) in roles {
        let names: BTreeSet<_> = schemas
            .iter()
            .map(|schema| schema["name"].as_str().expect("tool name").to_owned())
            .collect();
        assert_eq!(
            names.contains("proposal_refinement_demand_evidence"),
            matches!(role, "adversary" | "judge")
        );
        assert_eq!(
            names.contains("proposal_refinement_retry_evidence"),
            matches!(role, "advocate" | "judge")
        );
        assert_eq!(
            names.contains("proposal_refinement_resolve_evidence"),
            role == "judge"
        );
        assert_eq!(
            names.contains("proposal_refinement_withdraw_evidence"),
            role == "judge"
        );
    }

    // Each evidence tool's input surface is closed, and the fixture is where the
    // closure is written down. A blocklist of names that never existed proved
    // nothing; this fails on any property the fixture does not declare.
    let mut declared_tools: BTreeSet<String> = fixture
        .evidence_tool_input_properties
        .keys()
        .cloned()
        .collect();
    for (name, schema) in [
        ("proposal_refinement_demand_evidence", &demand),
        ("proposal_refinement_retry_evidence", &retry),
        ("proposal_refinement_resolve_evidence", &resolve),
        ("proposal_refinement_withdraw_evidence", &withdraw),
    ] {
        assert!(
            declared_tools.remove(name),
            "the fixture must declare the closed input-property set of {name}"
        );
        assert_eq!(
            keys(schema, &["inputSchema", "properties"]),
            fixture.evidence_tool_input_properties[name],
            "{name}'s input properties are closed; a new field here is a new \
             channel into the demand path and must be added to the fixture \
             deliberately, and only for a field that is not a free-text \
             question collection"
        );
    }
    assert!(
        declared_tools.is_empty(),
        "the fixture declares input properties for tools this test never checks: {declared_tools:?}"
    );
}
