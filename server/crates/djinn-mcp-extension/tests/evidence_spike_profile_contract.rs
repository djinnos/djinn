//! Exact public tool-name contract for Judge and evidence-spike profiles.
//!
//! The fixture deliberately pins both profiles together: a grounded evidence
//! spike gains only the constrained plan/executor controls while retaining its
//! Architect readers, and it never inherits the Judge's ordinary shell tool.
//!
//! The fixture is one of the artifacts derived from the MCP tool schemas
//! (`scripts/tool-goldens.manifest.json`). `make tool-goldens` refreshes it
//! together with every other one, by running this test with
//! `UPDATE_EVIDENCE_SPIKE_PROFILE_FIXTURE=1`. The behavioural assertions below
//! still run in write mode, so regenerating cannot launder a surface change
//! that violates the profile boundaries.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use djinn_mcp_extension::tool_defs::{tool_schemas_evidence_spike, tool_schemas_judge};
use serde::{Deserialize, Serialize};

const FIXTURE: &str = include_str!("fixtures/evidence_spike_profile.json");

/// Path of the committed fixture, relative to `CARGO_MANIFEST_DIR`.
const FIXTURE_REL: &str = "tests/fixtures/evidence_spike_profile.json";

/// Environment switch that turns this contract into its own generator, mirroring
/// `UPDATE_DJINN_MCP_SERVER_FIXTURE` in `djinn-control-plane`.
const UPDATE_ENV: &str = "UPDATE_EVIDENCE_SPIKE_PROFILE_FIXTURE";

const REGENERATE_HINT: &str = "\n\
    Regenerate EVERY derived MCP tool-schema artifact with one command, from the repository root:\n\
    \n    make tool-goldens\n\n\
    The full artifact set lives in scripts/tool-goldens.manifest.json.";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidenceSpikeProfileFixture {
    judge: BTreeSet<String>,
    evidence_spike: BTreeSet<String>,
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_REL)
}

/// Canonical on-disk form: pretty JSON with one trailing newline, so a rewrite
/// with no surface change is byte-identical to what is already committed.
fn render(fixture: &EvidenceSpikeProfileFixture) -> String {
    let mut rendered =
        serde_json::to_string_pretty(fixture).expect("profile fixture must serialize");
    rendered.push('\n');
    rendered
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
    let judge = tool_names(&tool_schemas_judge());
    let spike = tool_names(&tool_schemas_evidence_spike());

    if std::env::var(UPDATE_ENV).as_deref() == Ok("1") {
        let generated = EvidenceSpikeProfileFixture {
            judge: judge.clone(),
            evidence_spike: spike.clone(),
        };
        let path = fixture_path();
        std::fs::write(&path, render(&generated))
            .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    } else {
        let fixture: EvidenceSpikeProfileFixture = serde_json::from_str(FIXTURE)
            .expect("evidence spike profile fixture must be valid JSON");

        assert_eq!(
            judge, fixture.judge,
            "Judge tool names changed.{REGENERATE_HINT}"
        );
        assert_eq!(
            spike, fixture.evidence_spike,
            "evidence-spike tool names changed.{REGENERATE_HINT}"
        );
        assert_eq!(
            FIXTURE,
            render(&fixture),
            "evidence spike profile fixture is not in canonical form.{REGENERATE_HINT}"
        );
    }

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
