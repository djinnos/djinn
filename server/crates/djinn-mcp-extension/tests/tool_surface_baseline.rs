//! Regression contract for the advertised MCP tool surface.
//!
//! Update the checked-in baseline only through:
//!
//! `cargo run -p djinn-mcp-extension --bin regenerate_tool_surface_baseline`

use djinn_mcp_extension::tool_surface::{
    tool_surface_baseline_json, validate_operation_description_enums,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/tool_surface_baseline.json");

#[test]
fn generated_tool_surface_matches_reviewed_baseline() {
    // This is deliberately the same public generation API used by the
    // regeneration binary. Do not duplicate inventory collection or
    // canonicalization in this test.
    let generated = tool_surface_baseline_json();
    let generated_inventory: Vec<Value> = serde_json::from_str(&generated)
        .expect("canonical tool surface generation must produce a JSON array");
    let tool_count = generated_inventory.len();

    let operation_diagnostics = validate_operation_description_enums(&generated_inventory);
    assert!(
        operation_diagnostics.is_empty(),
        "operation-enum guard found {} issue(s) in generated inventory of {tool_count} unique tools:\n{}",
        operation_diagnostics.len(),
        operation_diagnostics
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );

    assert_eq!(
        generated.as_bytes(),
        FIXTURE.as_bytes(),
        "generated MCP tool surface differs from the reviewed fixture (generated unique tool count: {tool_count}). \
         Regenerate with: cargo run -p djinn-mcp-extension --bin regenerate_tool_surface_baseline"
    );
    assert!(
        generated.ends_with('\n'),
        "generated MCP tool surface must end with a trailing newline (generated unique tool count: {tool_count})"
    );
}
