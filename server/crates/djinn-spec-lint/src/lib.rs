//! Dependency-safe, deterministic proposal-body lint contracts and MDX registry parsing.
//!
//! This crate intentionally has no dependency on the control plane or database.

mod catalog;
mod parser;
mod types;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use catalog::{proposal_block_tags, registered_block_types};
pub use parser::{
    extract_custom_block_tags, parse_mdx_blocks, validate_block_content, validate_block_ids,
    validate_mdx_blocks, validate_question_form_placement,
    validate_question_form_placement_for_format,
};
pub use types::{
    BlockError, BodyFormat, ParsedProposalBlock, Severity, SkippedTier, SpanError, Utf8ByteSpan,
    Violation,
};

/// Stable V1 persistence and API contract. `checked_at` is supplied by the
/// caller; linting never reads a clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SpecLintResultV1 {
    pub linter_version: String,
    pub body_sha256: String,
    pub body_format: BodyFormat,
    pub checked_at: String,
    pub errors: Vec<Violation>,
    pub warnings: Vec<Violation>,
    pub skipped_tiers: Vec<SkippedTier>,
}

impl SpecLintResultV1 {
    pub const LINTER_VERSION: &'static str = "v1";

    pub fn new(body: &str, body_format: BodyFormat, checked_at: impl Into<String>) -> Self {
        let skipped_tiers = match body_format {
            BodyFormat::Markdown => vec![SkippedTier::body_format_markdown()],
            BodyFormat::Mdx => Vec::new(),
        };
        Self {
            linter_version: Self::LINTER_VERSION.into(),
            body_sha256: body_sha256(body),
            body_format,
            checked_at: checked_at.into(),
            errors: Vec::new(),
            warnings: Vec::new(),
            skipped_tiers,
        }
    }

    /// Reject result data whose spans cannot address the exact original body.
    pub fn validate_for_body(&self, body: &str) -> Result<(), SpanError> {
        for violation in self.errors.iter().chain(&self.warnings) {
            violation.span.validate_for(body)?;
        }
        Ok(())
    }

    /// Sort diagnostics by the persisted contract ordering.
    pub fn sort_violations(&mut self) {
        let order = |a: &Violation, b: &Violation| {
            a.span
                .start
                .cmp(&b.span.start)
                .then(a.span.end.cmp(&b.span.end))
                .then(a.code.cmp(&b.code))
        };
        self.errors.sort_by(order);
        self.warnings.sort_by(order);
    }
}

/// SHA-256 of the exact stored UTF-8 body bytes, encoded as lowercase hex.
pub fn body_sha256(body: &str) -> String {
    format!("{:x}", Sha256::digest(body.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_is_deterministic_and_uses_stable_spelling() {
        let result = SpecLintResultV1::new("é", BodyFormat::Markdown, "2026-01-02T03:04:05Z");
        assert_eq!(result.body_sha256, body_sha256("é"));
        assert_eq!(
            result.skipped_tiers,
            vec![SkippedTier::body_format_markdown()]
        );
        let encoded = serde_json::to_value(&result).unwrap();
        assert_eq!(encoded["body_format"], "markdown");
        assert_eq!(
            encoded["skipped_tiers"][0]["reason"],
            "BODY_FORMAT_MARKDOWN"
        );
        assert_eq!(encoded["checked_at"], "2026-01-02T03:04:05Z");
        assert_eq!(
            serde_json::from_value::<SpecLintResultV1>(encoded).unwrap(),
            result
        );
    }

    #[test]
    fn violations_and_spans_have_stable_serialization_and_utf8_guards() {
        let violation =
            Violation::new("éx", "MDX_PARSE_ERROR", Severity::Error, "bad", 0, 2).unwrap();
        let encoded = serde_json::to_value(&violation).unwrap();
        assert_eq!(encoded["severity"], "error");
        assert_eq!(encoded["span"], serde_json::json!({"start": 0, "end": 2}));
        assert!(matches!(
            Utf8ByteSpan::new("éx", 1, 2),
            Err(SpanError::NotUtf8Boundary { .. })
        ));
        assert!(matches!(
            Utf8ByteSpan::new("éx", 0, 4),
            Err(SpanError::OutOfBounds { .. })
        ));
        let mut result = SpecLintResultV1::new("éx", BodyFormat::Mdx, "fixed");
        result.errors.push(Violation {
            span: Utf8ByteSpan { start: 1, end: 2 },
            ..violation
        });
        assert!(result.validate_for_body("éx").is_err());
    }

    #[test]
    fn registered_parser_preserves_raw_content_and_attributes() {
        let body = "<Callout id=\"outer\" enabled>\nbefore\n<Callout id=\"inner\">nested</Callout>\nafter\n</Callout>";
        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].attributes.get("enabled").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            blocks[0].raw_content,
            "\nbefore\n<Callout id=\"inner\">nested</Callout>\nafter\n"
        );
        let expression = parse_mdx_blocks(r#"<Diagram id="d" source={{ "a": 1 }} />"#).unwrap();
        assert_eq!(expression[0].attributes["source"], r#"{ "a": 1 }"#);
    }

    #[test]
    fn parser_validation_handles_unknown_unclosed_empty_and_diagram_safety() {
        assert_eq!(
            validate_mdx_blocks("<RichText><GhostBlock /></RichText>"),
            Err(BlockError::UnknownBlock("GhostBlock".into()))
        );
        assert_eq!(
            parse_mdx_blocks("<Diagram id=\"d\">"),
            Err(BlockError::UnclosedBlock("Diagram".into()))
        );
        assert!(parse_mdx_blocks("   ").unwrap().is_empty());
        assert_eq!(
            validate_mdx_blocks(r#"<Diagram id="d" source="" />"#),
            Err(BlockError::EmptyDiagram("d".into()))
        );
        assert!(validate_mdx_blocks("<Diagram id=\"d\">flowchart LR; A-->B</Diagram>").is_ok());

        let empty = parse_mdx_blocks("<Callout id=\"empty\" />").unwrap();
        assert!(
            validate_block_content(&empty)
                .unwrap_err()
                .contains("empty")
        );
    }
}
