//! Proposal MDX block registry and introspection tool.
//!
//! This module is the Rust source for the v1 proposal block contract: each
//! stable block type maps to the MDX tag clients should emit plus the field
//! schema workers can use when validating or generating proposal bodies.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ProposalBlockFieldSchema {
    /// Primitive schema kind: `string`, `boolean`, `object`, or `array`.
    #[serde(rename = "type")]
    pub field_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<BTreeMap<&'static str, ProposalBlockFieldSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<ProposalBlockFieldSchema>>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ProposalBlockDefinition {
    /// Stable proposal block type identifier, e.g. `annotated-code`.
    #[serde(rename = "type")]
    pub block_type: &'static str,
    /// MDX component tag, e.g. `AnnotatedCode`.
    pub tag: &'static str,
    /// Field schema keyed by field name.
    pub fields: BTreeMap<&'static str, ProposalBlockFieldSchema>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalBlocksParams {}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ProposalBlocksResponse {
    pub blocks: BTreeMap<&'static str, ProposalBlockDefinition>,
}

fn string_field() -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "string",
        enum_values: None,
        fields: None,
        items: None,
    }
}

fn boolean_field() -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "boolean",
        enum_values: None,
        fields: None,
        items: None,
    }
}

fn enum_string_field(values: Vec<&'static str>) -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "string",
        enum_values: Some(values),
        fields: None,
        items: None,
    }
}

fn object_field(
    fields: BTreeMap<&'static str, ProposalBlockFieldSchema>,
) -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "object",
        enum_values: None,
        fields: Some(fields),
        items: None,
    }
}

fn array_field(items: ProposalBlockFieldSchema) -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "array",
        enum_values: None,
        fields: None,
        items: Some(Box::new(items)),
    }
}

fn fields(
    entries: Vec<(&'static str, ProposalBlockFieldSchema)>,
) -> BTreeMap<&'static str, ProposalBlockFieldSchema> {
    entries.into_iter().collect()
}

fn block(
    block_type: &'static str,
    tag: &'static str,
    fields: BTreeMap<&'static str, ProposalBlockFieldSchema>,
) -> ProposalBlockDefinition {
    ProposalBlockDefinition {
        block_type,
        tag,
        fields,
    }
}

pub static PROPOSAL_BLOCK_REGISTRY: LazyLock<BTreeMap<&'static str, ProposalBlockDefinition>> =
    LazyLock::new(|| {
        BTreeMap::from([
            (
                "rich-text",
                block(
                    "rich-text",
                    "RichText",
                    fields(vec![("content", string_field())]),
                ),
            ),
            (
                "diagram",
                block(
                    "diagram",
                    "Diagram",
                    fields(vec![
                        (
                            "type",
                            enum_string_field(vec!["mermaid", "plantuml", "svg"]),
                        ),
                        ("source", string_field()),
                    ]),
                ),
            ),
            (
                "annotated-code",
                block(
                    "annotated-code",
                    "AnnotatedCode",
                    fields(vec![
                        ("language", string_field()),
                        ("code", string_field()),
                        (
                            "annotations",
                            array_field(object_field(fields(vec![
                                ("line", string_field()),
                                ("note", string_field()),
                            ]))),
                        ),
                    ]),
                ),
            ),
            (
                "data-model",
                block(
                    "data-model",
                    "DataModel",
                    fields(vec![
                        ("name", string_field()),
                        (
                            "fields",
                            array_field(object_field(fields(vec![
                                ("name", string_field()),
                                ("type", string_field()),
                                ("optional", boolean_field()),
                                ("description", string_field()),
                            ]))),
                        ),
                    ]),
                ),
            ),
            (
                "api-endpoint",
                block(
                    "api-endpoint",
                    "ApiEndpoint",
                    fields(vec![
                        ("method", string_field()),
                        ("path", string_field()),
                        ("description", string_field()),
                        ("request_schema", string_field()),
                        ("response_schema", string_field()),
                    ]),
                ),
            ),
            (
                "decisions",
                block(
                    "decisions",
                    "Decisions",
                    fields(vec![(
                        "items",
                        array_field(object_field(fields(vec![
                            ("decision", string_field()),
                            ("rationale", string_field()),
                            ("status", string_field()),
                        ]))),
                    )]),
                ),
            ),
            (
                "file-tree",
                block(
                    "file-tree",
                    "FileTree",
                    fields(vec![
                        ("root", string_field()),
                        (
                            "entries",
                            array_field(object_field(fields(vec![
                                ("path", string_field()),
                                ("kind", enum_string_field(vec!["file", "dir"])),
                            ]))),
                        ),
                    ]),
                ),
            ),
            (
                "question-form",
                block(
                    "question-form",
                    "QuestionForm",
                    fields(vec![
                        ("title", string_field()),
                        (
                            "questions",
                            array_field(object_field(fields(vec![
                                ("question", string_field()),
                                ("kind", enum_string_field(vec!["text", "single", "multi"])),
                                ("options", array_field(string_field())),
                            ]))),
                        ),
                    ]),
                ),
            ),
        ])
    });

pub fn proposal_block_registry() -> BTreeMap<&'static str, ProposalBlockDefinition> {
    PROPOSAL_BLOCK_REGISTRY.clone()
}

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
