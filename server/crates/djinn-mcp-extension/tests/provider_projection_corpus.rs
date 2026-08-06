//! Freshness contract for the `djinn-provider` tool-schema projection corpus.
//!
//! `djinn-provider` cannot call the role schema helpers directly — it is a
//! runtime dependency of this crate, so a dev-dependency back would be a cycle
//! — so its corpus under
//! `crates/djinn-provider/tests/fixtures/tool_schema_projection/builtin/` is a
//! committed copy of this crate's role snapshots. Nothing enforced that copy
//! before, and the `worker` / `planner` / `reviewer` members had already
//! drifted away from the live surface. This makes the copy an enforced
//! artifact instead of a hopeful one.
//!
//! It deliberately lives in its own integration target rather than beside the
//! snapshots in `src/tests/schema_tests.rs`: `make tool-goldens` regenerates
//! those snapshots by RUNNING that module, and a downstream freshness
//! assertion inside it would fail the regeneration step that is about to fix
//! it. Producers must never run checks on artifacts written later in the plan.

/// Fixture path relative to `CARGO_MANIFEST_DIR`, and the snapshot it mirrors.
const PROVIDER_ROLE_CORPUS: [(&str, &str); 5] = [
    (
        "../djinn-provider/tests/fixtures/tool_schema_projection/builtin/worker.json",
        "djinn_mcp_extension__tests__schema_tests__worker_tool_schemas.snap",
    ),
    (
        "../djinn-provider/tests/fixtures/tool_schema_projection/builtin/planner.json",
        "djinn_mcp_extension__tests__schema_tests__planner_tool_schemas.snap",
    ),
    (
        "../djinn-provider/tests/fixtures/tool_schema_projection/builtin/lead.json",
        "djinn_mcp_extension__tests__schema_tests__lead_tool_schemas.snap",
    ),
    (
        "../djinn-provider/tests/fixtures/tool_schema_projection/builtin/reviewer.json",
        "djinn_mcp_extension__tests__schema_tests__reviewer_tool_schemas.snap",
    ),
    (
        "../djinn-provider/tests/fixtures/tool_schema_projection/builtin/architect.json",
        "djinn_mcp_extension__tests__schema_tests__architect_tool_schemas.snap",
    ),
];

const TOOL_GOLDEN_HINT: &str = "\n\n\
    ── this fixture is an MCP tool-schema golden ───────────────────────────\n\
    Regenerate EVERY derived tool-schema artifact with one command, from the\n\
    repository root:\n\
    \n    make tool-goldens\n\n\
    The full artifact set lives in scripts/tool-goldens.manifest.json.\n";

/// Strip insta's YAML frontmatter. Mirrors `instaSnapshotBody` in
/// `scripts/lib/tool-goldens.mjs`, which is what `make tool-goldens` runs.
fn insta_snapshot_body(raw: &str) -> String {
    let body = raw
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map_or(raw, |(_frontmatter, body)| body);
    format!("{}\n", body.trim_end())
}

#[test]
fn provider_projection_corpus_matches_the_role_snapshots() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let snapshot_dir = manifest_dir.join("src/tests/snapshots");

    for (fixture_rel, snapshot_name) in PROVIDER_ROLE_CORPUS {
        let snapshot_path = snapshot_dir.join(snapshot_name);
        let raw = std::fs::read_to_string(&snapshot_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", snapshot_path.display()));
        let expected = insta_snapshot_body(&raw);

        let fixture_path = manifest_dir.join(fixture_rel);
        let committed = std::fs::read_to_string(&fixture_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", fixture_path.display()));

        assert_eq!(
            committed, expected,
            "{fixture_rel} has drifted from {snapshot_name}.{TOOL_GOLDEN_HINT}"
        );
    }
}
