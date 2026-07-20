use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use super::types::ProposalBlockDefinition;

const BLOCKS: &[(&str, &str)] = &[
    ("annotated-code", "AnnotatedCode"),
    ("api-endpoint", "ApiEndpoint"),
    ("callout", "Callout"),
    ("checklist", "Checklist"),
    ("columns", "Columns"),
    ("decisions", "Decisions"),
    ("diagram", "Diagram"),
    ("diff", "Diff"),
    ("file-tree", "FileTree"),
    ("json-explorer", "JsonExplorer"),
    ("question-form", "QuestionForm"),
    ("rich-text", "RichText"),
    ("tabs", "Tabs"),
    ("wireframe", "Wireframe"),
];

static REGISTRY: LazyLock<BTreeMap<&'static str, ProposalBlockDefinition>> = LazyLock::new(|| {
    BLOCKS
        .iter()
        .map(|&(block_type, tag)| {
            (
                block_type,
                ProposalBlockDefinition {
                    block_type,
                    tag,
                    description: None,
                },
            )
        })
        .collect()
});
pub(crate) fn proposal_block_definition_for_tag(
    tag: &str,
) -> Option<&'static ProposalBlockDefinition> {
    REGISTRY.values().find(|definition| definition.tag == tag)
}
pub fn proposal_block_tags() -> HashSet<&'static str> {
    REGISTRY.values().map(|definition| definition.tag).collect()
}
pub fn registered_block_types() -> Vec<(&'static str, &'static str)> {
    BLOCKS.to_vec()
}
