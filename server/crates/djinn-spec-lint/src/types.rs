use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Errors produced by registered proposal-block parsing and validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockError {
    UnknownBlock(String),
    MissingId(String),
    DuplicateId(String),
    UnclosedBlock(String),
    ParseError(String),
    EmptyDiagram(String),
}

impl std::fmt::Display for BlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownBlock(tag) => write!(f, "Unknown MDX block tag: '{tag}'"),
            Self::MissingId(tag) => write!(f, "{tag} block is missing a required `id` attribute"),
            Self::DuplicateId(id) => write!(f, "duplicate block id: `{id}`"),
            Self::UnclosedBlock(tag) => {
                write!(f, "unclosed <{tag}> block (no closing </{tag}> found)")
            }
            Self::ParseError(message) => write!(f, "block parser error: {message}"),
            Self::EmptyDiagram(id) => write!(
                f,
                "Diagram block `{id}` has no source — provide a non-empty `source` \
                 (e.g. `source={{`flowchart LR; A-->B`}}`) or block content"
            ),
        }
    }
}
impl std::error::Error for BlockError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ParsedProposalBlock {
    pub block_type: String,
    pub tag: String,
    pub id: String,
    pub attributes: HashMap<String, String>,
    /// Byte-identical children sliced from the source body.
    pub raw_content: String,
}
impl ParsedProposalBlock {
    pub fn block_id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProposalBlockDefinition {
    pub(crate) block_type: &'static str,
    pub(crate) tag: &'static str,
    pub(crate) description: Option<&'static str>,
}

/// Body encoding supplied by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BodyFormat {
    Markdown,
    Mdx,
}

/// Stable diagnostic severity.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}

/// A half-open UTF-8 byte range in the original body.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub struct Utf8ByteSpan {
    #[schemars(with = "i64")]
    pub start: usize,
    #[schemars(with = "i64")]
    pub end: usize,
}

impl Utf8ByteSpan {
    pub fn new(body: &str, start: usize, end: usize) -> Result<Self, SpanError> {
        if start > end || end > body.len() {
            return Err(SpanError::OutOfBounds {
                start,
                end,
                len: body.len(),
            });
        }
        if !body.is_char_boundary(start) || !body.is_char_boundary(end) {
            return Err(SpanError::NotUtf8Boundary { start, end });
        }
        Ok(Self { start, end })
    }
    pub fn validate_for(&self, body: &str) -> Result<(), SpanError> {
        Self::new(body, self.start, self.end).map(|_| ())
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanError {
    OutOfBounds {
        start: usize,
        end: usize,
        len: usize,
    },
    NotUtf8Boundary {
        start: usize,
        end: usize,
    },
}
impl std::fmt::Display for SpanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid UTF-8 byte span: {self:?}")
    }
}
impl std::error::Error for SpanError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Violation {
    pub code: String,
    pub severity: Severity,
    pub message: String,
    pub span: Utf8ByteSpan,
}
impl Violation {
    pub fn new(
        body: &str,
        code: impl Into<String>,
        severity: Severity,
        message: impl Into<String>,
        start: usize,
        end: usize,
    ) -> Result<Self, SpanError> {
        Ok(Self {
            code: code.into(),
            severity,
            message: message.into(),
            span: Utf8ByteSpan::new(body, start, end)?,
        })
    }
}

/// A deterministic record of a lint tier that did not run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkippedTier {
    pub tier: String,
    pub reason: String,
}
impl SkippedTier {
    pub fn body_format_markdown() -> Self {
        Self {
            tier: "mdx_structure".into(),
            reason: "BODY_FORMAT_MARKDOWN".into(),
        }
    }
}
