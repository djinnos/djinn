//! Tests for the LSP tool schema surface.
//!
//! Moved from `djinn-agent::extension::tests::lsp_dispatch_tests` during
//! the Phase 4 extraction — these test `crate::tool_defs::tool_lsp` directly.

use crate::tool_defs::tool_lsp;

#[test]
fn tool_lsp_schema_exposes_symbol_filters() {
    let tool = tool_lsp();
    let schema = serde_json::to_value(&tool).unwrap();
    let input_schema = &schema["inputSchema"]["properties"];
    assert!(input_schema.get("symbol").is_some());
    assert!(input_schema.get("depth").is_some());
    assert!(input_schema.get("kind").is_some());
    assert!(input_schema.get("name_filter").is_some());
}
