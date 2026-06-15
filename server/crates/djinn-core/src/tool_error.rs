//! Shared structured tool-error envelope and construction-time taxonomy.
//!
//! This is the reusable shape provider/control-plane surfaces use when a tool
//! failure needs to carry a machine-readable class as well as compact
//! agent-facing detail.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Machine-readable `ToolError.error_class` taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    NotFound,
    ConflictRecoverable,
    Validation,
    Permission,
    RateLimited,
    Transient,
    Internal,
}

impl ErrorClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::ConflictRecoverable => "conflict_recoverable",
            Self::Validation => "validation",
            Self::Permission => "permission",
            Self::RateLimited => "rate_limited",
            Self::Transient => "transient",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Structured error envelope returned to agents/operators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolError {
    /// Human-readable message describing what went wrong.
    pub error: String,
    /// Machine-readable class assigned at construction time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<ErrorClass>,
    /// Numeric HTTP status or coarse symbolic status when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Logical operation or upstream method that failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Targeted resource or upstream path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Bounded upstream response/detail excerpt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Actionable next step for the caller.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl ToolError {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            error_class: None,
            status: None,
            method: None,
            path: None,
            body: None,
            hint: None,
        }
    }

    #[must_use]
    pub fn with_error_class(mut self, error_class: ErrorClass) -> Self {
        self.error_class = Some(error_class);
        self
    }

    #[must_use]
    pub fn with_status(mut self, status: impl Into<String>) -> Self {
        self.status = Some(status.into());
        self
    }

    #[must_use]
    pub fn with_http_status(self, status: u16) -> Self {
        self.with_status(status.to_string())
    }

    #[must_use]
    pub fn with_method(mut self, method: impl Into<String>) -> Self {
        self.method = Some(method.into());
        self
    }

    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Compact deterministic rendering for agent/operator-facing text.
    pub fn compact(&self) -> String {
        let method = self.method.as_deref().unwrap_or("unknown");
        let path = self.path.as_deref().unwrap_or("unknown");
        let status = self.status.as_deref().unwrap_or("none");
        let error_class = self
            .error_class
            .map(|class| class.as_str())
            .unwrap_or("unclassified");
        let hint = self.hint.as_deref().unwrap_or("");
        let mut rendered = format!(
            "Tool error method={method} path={path} status={status} error_class={error_class} hint={hint} error={}",
            self.error
        );
        if let Some(body) = &self.body {
            rendered.push_str(" body=");
            rendered.push_str(body);
        }
        rendered
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.compact())
    }
}

impl std::error::Error for ToolError {}
