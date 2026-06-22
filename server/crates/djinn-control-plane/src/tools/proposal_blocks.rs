//! Proposal MDX block registry and introspection tool.
//!
//! This module is the Rust source for the v1 proposal block contract: each
//! stable block type maps to the MDX tag clients should emit plus the field
//! schema workers can use when validating or generating proposal bodies.
//!
//! It is a thin facade over a `proposal_blocks/` module split (this crate's
//! established `agent_tools.rs` + `agent_tools/` convention):
//!   * [`types`]   — the registry struct shapes (back the `tool_schemas` snapshot),
//!   * [`macros`]  — the `define_blocks!` macro + field-schema builder helpers,
//!   * [`catalog`] — the single `define_blocks!` invocation for all v1 blocks,
//!   * [`parser`]  — the MDX-AST parse / validate functions.
//!
//! Every public symbol the rest of the crate imports today is re-exported here,
//! so downstream `crate::tools::proposal_blocks::…` import paths are unchanged.

use rmcp::{Json, handler::server::wrapper::Parameters, tool, tool_router};

use crate::server::DjinnMcpServer;

mod catalog;
mod macros;
mod parser;
mod types;

#[cfg(test)]
mod tests;

pub use catalog::{
    BlockType, CANONICAL_BLOCK_TYPES, PROPOSAL_BLOCK_REGISTRY, proposal_block_definition_for_tag,
    proposal_block_registry, proposal_block_tags,
};
pub use parser::{
    extract_custom_block_tags, parse_mdx_blocks, validate_block_ids, validate_mdx_blocks,
    validate_question_form_placement, validate_question_form_placement_for_format,
};
pub use types::{
    BlockError, BlockRegistry, ParsedProposalBlock, ProposalBlockDefinition,
    ProposalBlockFieldSchema, ProposalBlocksParams, ProposalBlocksResponse,
};

#[tool_router(router = proposal_blocks_tool_router, vis = "pub")]
impl DjinnMcpServer {
    #[tool(
        description = "Return the v1 proposal MDX block registry, including stable block types, MDX tags, and field schemas.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn proposal_blocks(
        &self,
        Parameters(_): Parameters<ProposalBlocksParams>,
    ) -> Json<ProposalBlocksResponse> {
        Json(ProposalBlocksResponse {
            blocks: proposal_block_registry(),
        })
    }
}
