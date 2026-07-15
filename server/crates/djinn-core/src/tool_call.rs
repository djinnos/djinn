//! Transport-neutral MCP tool-call outcomes and compatibility metadata.
//!
//! Compatibility policy is implemented by the MCP extension, while this module
//! deliberately contains only the stable wire contract shared by server layers.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Schema emitted for Djinn tool-call compatibility metadata.
pub const DJINN_TOOL_CALL_METADATA_SCHEMA_VERSION: u32 = 1;

/// Result returned by a tool invocation before provider-specific rendering.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallOutcome {
    Success {
        value: Value,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<CompatibilityMetadata>,
    },
    Failure(ToolCallFailure),
}

impl ToolCallOutcome {
    /// Preserve an ordinary handler result while making the transport explicit.
    pub fn from_result(result: Result<Value, String>) -> Self {
        match result {
            Ok(value) => Self::Success {
                value,
                warnings: Vec::new(),
            },
            Err(message) => Self::Failure(ToolCallFailure::Message(message)),
        }
    }
}

/// Failure categories supported by the typed tool-call transport.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallFailure {
    Message(String),
    Structured {
        code: ToolCallErrorCode,
        message: String,
        data: CompatibilityMetadata,
    },
}

/// Stable error codes for compatibility failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallErrorCode {
    RemovedSurface,
    InvalidCompatCall,
}

/// Compatibility event code, including non-fatal deprecation warnings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityCode {
    DeprecatedSurface,
    RemovedSurface,
    InvalidCompatCall,
}

/// MCP surface to which compatibility metadata applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    Tool,
    Parameter,
}

/// Reason a stale call cannot be safely normalized.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvalidCompatReason {
    UnsafeForwarding,
    AmbiguousParameter,
    UnsafeOmission,
}

/// Djinn-owned remedy catalog. Text is selected by this closed enum only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustedRemedyCode {
    CallReplacementTool,
    UseReplacementParameter,
    OmitRemovedParameter,
    NoReplacement,
}

/// A closed remedy and its Djinn-compiled text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrustedRemedy {
    pub code: TrustedRemedyCode,
}

impl TrustedRemedy {
    pub const fn new(code: TrustedRemedyCode) -> Self {
        Self { code }
    }

    pub const fn text(self) -> &'static str {
        match self.code {
            TrustedRemedyCode::CallReplacementTool => {
                "Call the replacement tool named in replacement_tool."
            }
            TrustedRemedyCode::UseReplacementParameter => {
                "Use the replacement parameter named in replacement_parameter."
            }
            TrustedRemedyCode::OmitRemovedParameter => "Omit the removed parameter.",
            TrustedRemedyCode::NoReplacement => "There is no replacement for this surface.",
        }
    }
}

impl Serialize for TrustedRemedy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Wire {
            code: TrustedRemedyCode,
            text: &'static str,
        }
        Wire {
            code: self.code,
            text: self.text(),
        }
        .serialize(serializer)
    }
}

/// Deserialization accepts the catalog code but verifies the compiled text.
impl<'de> Deserialize<'de> for TrustedRemedy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            code: TrustedRemedyCode,
            text: String,
        }
        let wire = Wire::deserialize(deserializer)?;
        let remedy = Self::new(wire.code);
        if wire.text != remedy.text() {
            return Err(serde::de::Error::custom(
                "remedy text is not Djinn-authored",
            ));
        }
        Ok(remedy)
    }
}

/// Structured metadata retained from normalization through final rendering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CompatibilityMetadata {
    pub schema_version: u32,
    pub code: CompatibilityCode,
    pub surface_kind: SurfaceKind,
    pub old_name: String,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_parameter: Option<String>,
    pub introduced_in: String,
    pub remove_after: String,
    pub remedy: TrustedRemedy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<InvalidCompatReason>,
}
