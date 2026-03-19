//! Prompt templates used by MCP workers.

pub const MEMORY_L0_ABSTRACT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/prompts/memory_l0_abstract.md"
));

pub const MEMORY_L1_OVERVIEW: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/prompts/memory_l1_overview.md"
));
