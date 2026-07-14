//! Typed values for the `note_revision_events` audit ledger.
//!
//! These are deliberately database-neutral: the later transactional mutation
//! boundary owns sequence allocation and persistence, while callers can only
//! construct attribution, provenance, and reasons in their validated forms.

use std::fmt;

use djinn_core::auth_context::{TrustedRevisionCallerContext, TrustedRevisionPrincipal};
use serde::{Deserialize, Serialize};

/// The closed set of changes represented by the note revision ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteRevisionEventKind {
    Created,
    Updated,
    Deleted,
    ConfidenceChanged,
    ExtractionSkipped,
}

impl NoteRevisionEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Deleted => "deleted",
            Self::ConfidenceChanged => "confidence_changed",
            Self::ExtractionSkipped => "extraction_skipped",
        }
    }
}

/// The trusted principal category which caused a revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteRevisionActorKind {
    Human,
    Agent,
    System,
}

impl NoteRevisionActorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::System => "system",
        }
    }
}

/// Closed identities for repository-owned system writers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteRevisionSubsystem {
    Mcp,
    Dedup,
    Consolidation,
    Enrichment,
    Extraction,
}

impl NoteRevisionSubsystem {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Dedup => "dedup",
            Self::Consolidation => "consolidation",
            Self::Enrichment => "enrichment",
            Self::Extraction => "extraction",
        }
    }
}

/// Why a trusted ledger value could not be constructed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NoteRevisionValidationError {
    #[error("note revision reason must not be blank")]
    BlankReason,
    #[error("{field} must not be blank")]
    BlankField { field: &'static str },
    #[error("{field} must be absent for this actor kind")]
    UnexpectedField { field: &'static str },
}

fn required(
    field: &'static str,
    value: impl Into<String>,
) -> Result<String, NoteRevisionValidationError> {
    let value = value.into();
    if value.trim().is_empty() {
        Err(NoteRevisionValidationError::BlankField { field })
    } else {
        Ok(value)
    }
}

/// A mandatory, normalized human-readable explanation for a ledger event.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct NoteRevisionReason(String);

impl NoteRevisionReason {
    /// Reject blank input and retain its trimmed representation.
    pub fn new(reason: impl Into<String>) -> Result<Self, NoteRevisionValidationError> {
        let reason = reason.into();
        let trimmed = reason.trim();
        if trimmed.is_empty() {
            return Err(NoteRevisionValidationError::BlankReason);
        }
        Ok(Self(trimmed.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for NoteRevisionReason {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for NoteRevisionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Trusted attribution, whose shape exactly mirrors the database constraint.
///
/// This intentionally has no `Deserialize` implementation. Repository callers
/// must construct it with the trusted constructors instead of accepting raw
/// caller-supplied attribution at a wire boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrustedNoteRevisionAttribution {
    actor_kind: NoteRevisionActorKind,
    actor_id: Option<String>,
    subsystem: Option<String>,
}

impl TrustedNoteRevisionAttribution {
    pub fn human(actor_id: impl Into<String>) -> Result<Self, NoteRevisionValidationError> {
        Ok(Self {
            actor_kind: NoteRevisionActorKind::Human,
            actor_id: Some(required("actor_id", actor_id)?),
            subsystem: None,
        })
    }

    pub fn agent(actor_id: impl Into<String>) -> Result<Self, NoteRevisionValidationError> {
        Ok(Self {
            actor_kind: NoteRevisionActorKind::Agent,
            actor_id: Some(required("actor_id", actor_id)?),
            subsystem: None,
        })
    }

    pub fn system(subsystem: NoteRevisionSubsystem) -> Self {
        Self {
            actor_kind: NoteRevisionActorKind::System,
            actor_id: None,
            subsystem: Some(subsystem.as_str().to_owned()),
        }
    }

    pub const fn actor_kind(&self) -> NoteRevisionActorKind {
        self.actor_kind
    }

    pub fn actor_id(&self) -> Option<&str> {
        self.actor_id.as_deref()
    }

    pub fn subsystem(&self) -> Option<&str> {
        self.subsystem.as_deref()
    }
}

/// Optional trusted execution context attached to an event.
///
/// This intentionally has no `Deserialize` implementation: `new` normalizes
/// optional IDs and rejects blank values before persistence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TrustedNoteRevisionProvenance {
    session_id: Option<String>,
    task_id: Option<String>,
    task_run_id: Option<String>,
}

impl TrustedNoteRevisionProvenance {
    pub fn new(
        session_id: Option<String>,
        task_id: Option<String>,
        task_run_id: Option<String>,
    ) -> Result<Self, NoteRevisionValidationError> {
        fn optional(
            field: &'static str,
            value: Option<String>,
        ) -> Result<Option<String>, NoteRevisionValidationError> {
            value.map(|value| required(field, value)).transpose()
        }
        Ok(Self {
            session_id: optional("session_id", session_id)?,
            task_id: optional("task_id", task_id)?,
            task_run_id: optional("task_run_id", task_run_id)?,
        })
    }

    pub const fn is_empty(&self) -> bool {
        self.session_id.is_none() && self.task_id.is_none() && self.task_run_id.is_none()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn task_id(&self) -> Option<&str> {
        self.task_id.as_deref()
    }

    pub fn task_run_id(&self) -> Option<&str> {
        self.task_run_id.as_deref()
    }
}

/// Convert only the server-owned dispatch context into persisted revision
/// values. MCP wire parameters cannot construct either trusted value.
impl TryFrom<&TrustedRevisionCallerContext> for TrustedNoteRevisionAttribution {
    type Error = NoteRevisionValidationError;

    fn try_from(context: &TrustedRevisionCallerContext) -> Result<Self, Self::Error> {
        match context.principal() {
            TrustedRevisionPrincipal::Human { user_id } => Self::human(user_id.clone()),
            TrustedRevisionPrincipal::Agent { agent_id } => Self::agent(agent_id.clone()),
        }
    }
}

impl TryFrom<&TrustedRevisionCallerContext> for TrustedNoteRevisionProvenance {
    type Error = NoteRevisionValidationError;

    fn try_from(context: &TrustedRevisionCallerContext) -> Result<Self, Self::Error> {
        Self::new(
            context.session_id().map(ToOwned::to_owned),
            context.task_id().map(ToOwned::to_owned),
            context.task_run_id().map(ToOwned::to_owned),
        )
    }
}

/// Full before/after state carried by a revision event. Content is intentionally
/// absent for confidence-only and extraction-skipped events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteRevisionSnapshot {
    pub content_before: Option<String>,
    pub content_after: Option<String>,
    pub confidence_before: Option<f64>,
    pub confidence_after: Option<f64>,
}

/// Input for the future append-only mutation boundary.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoteRevisionEventInput {
    pub id: String,
    pub project_id: String,
    pub note_id: Option<String>,
    pub note_seq: Option<i64>,
    pub event_kind: NoteRevisionEventKind,
    pub snapshot: NoteRevisionSnapshot,
    pub attribution: TrustedNoteRevisionAttribution,
    pub provenance: TrustedNoteRevisionProvenance,
    pub reason: NoteRevisionReason,
}

/// A persisted ledger row, including the server-assigned stable cursor time.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoteRevisionEventRow {
    pub id: String,
    pub project_id: String,
    pub note_id: Option<String>,
    pub note_seq: Option<i64>,
    pub event_kind: NoteRevisionEventKind,
    pub snapshot: NoteRevisionSnapshot,
    pub attribution: TrustedNoteRevisionAttribution,
    pub provenance: TrustedNoteRevisionProvenance,
    pub reason: NoteRevisionReason,
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_is_trimmed_and_blank_rejected() {
        assert_eq!(
            NoteRevisionReason::new("  explain change \n")
                .unwrap()
                .as_str(),
            "explain change"
        );
        assert_eq!(
            NoteRevisionReason::new("\t ").unwrap_err(),
            NoteRevisionValidationError::BlankReason
        );
    }

    #[test]
    fn attribution_and_provenance_reject_blank_values() {
        assert!(TrustedNoteRevisionAttribution::agent(" ").is_err());
        assert!(TrustedNoteRevisionProvenance::new(Some("".into()), None, None).is_err());
        assert_eq!(
            TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Extraction).subsystem(),
            Some("extraction")
        );
    }

    #[test]
    fn server_owned_context_converts_human_and_agent_principals() {
        let human = TrustedRevisionCallerContext::authenticated_human("user-1").unwrap();
        let attribution = TrustedNoteRevisionAttribution::try_from(&human).unwrap();
        let provenance = TrustedNoteRevisionProvenance::try_from(&human).unwrap();
        assert_eq!(attribution.actor_kind(), NoteRevisionActorKind::Human);
        assert_eq!(attribution.actor_id(), Some("user-1"));
        assert!(provenance.is_empty());

        let agent = TrustedRevisionCallerContext::authenticated_agent("worker")
            .unwrap()
            .with_execution_provenance(
                Some("session".into()),
                Some("task".into()),
                Some("run".into()),
            );
        let attribution = TrustedNoteRevisionAttribution::try_from(&agent).unwrap();
        let provenance = TrustedNoteRevisionProvenance::try_from(&agent).unwrap();
        assert_eq!(attribution.actor_kind(), NoteRevisionActorKind::Agent);
        assert_eq!(attribution.actor_id(), Some("worker"));
        assert_eq!(provenance.session_id(), Some("session"));
        assert_eq!(provenance.task_id(), Some("task"));
        assert_eq!(provenance.task_run_id(), Some("run"));
    }
}
