//! Regenerate the checked-in MCP tool-surface review baseline.
//!
//! Run with:
//!
//! ```text
//! cargo run -p djinn-mcp-extension --bin regenerate_tool_surface_baseline
//! ```

use djinn_mcp_extension::tool_surface::tool_surface_baseline_json;

const FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/tool_surface_baseline.json"
);

fn main() {
    // This is deliberately the same public generation API used by the
    // integration regression test. Do not duplicate inventory collection or
    // canonicalization here.
    let baseline = tool_surface_baseline_json();
    let tool_count = serde_json::from_str::<Vec<serde_json::Value>>(&baseline)
        .expect("canonical tool surface must be a JSON array")
        .len();

    std::fs::write(FIXTURE_PATH, baseline)
        .unwrap_or_else(|error| panic!("failed to write {FIXTURE_PATH}: {error}"));

    eprintln!("regenerated {tool_count} MCP tools at {FIXTURE_PATH}");
}
