//! Shared additive V1 extension-load diagnostic contract.
//!
//! This module defines the wire/domain model used by agent, database, control-plane,
//! and doctor code to agree on the shape of an extension-load failure. Writers emit
//! schema version 1; readers tolerate unknown future object fields via serde's default
//! deserialization behavior.

use serde::{Deserialize, Serialize};

/// Fixed schema version for `ExtensionLoadDiagnosticV1` writers.
pub const EXTENSION_LOAD_DIAGNOSTIC_V1_SCHEMA_VERSION: i32 = 1;

/// Source of the failing extension load.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionLoadSourceKind {
    /// Project-configured MCP server (e.g. `tools/list` or handshake failures).
    ProjectMcp,
    /// Project-configured skill declared in the workspace manifest.
    ProjectSkill,
}

impl ExtensionLoadSourceKind {
    /// Canonical snake_case string representation used by the database and JSON contract.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ProjectMcp => "project_mcp",
            Self::ProjectSkill => "project_skill",
        }
    }
}

/// Phase of the extension-load lifecycle where the failure was observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionLoadPhase {
    /// Resolving a placeholder in the extension configuration.
    PlaceholderResolution,
    /// Starting the extension process or runtime.
    ProcessStart,
    /// Transport-layer connection (stdio, SSE, streamable HTTP, etc.).
    Transport,
    /// MCP or extension handshake / initialization.
    Handshake,
    /// Listing or validating tools/capabilities after connection.
    ToolsList,
    /// Parsing skill frontmatter.
    Frontmatter,
    /// A declared skill file is missing.
    MissingFile,
    /// Skill manifest has drifted from the loaded file.
    ManifestDrift,
}

impl ExtensionLoadPhase {
    /// Canonical snake_case string representation used by the database and JSON contract.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PlaceholderResolution => "placeholder_resolution",
            Self::ProcessStart => "process_start",
            Self::Transport => "transport",
            Self::Handshake => "handshake",
            Self::ToolsList => "tools_list",
            Self::Frontmatter => "frontmatter",
            Self::MissingFile => "missing_file",
            Self::ManifestDrift => "manifest_drift",
        }
    }
}

/// Severity of the extension-load failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionLoadSeverity {
    /// Degraded capability; execution may continue without the extension.
    Warning,
    /// Required extension failed; the load attempt cannot fully succeed.
    Error,
}

impl ExtensionLoadSeverity {
    /// Canonical snake_case string representation used by the database and JSON contract.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// Djinn-authored remedy selection for an extension-load failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionLoadRemedyCode {
    /// Check a placeholder value in extension configuration.
    CheckPlaceholder,
    /// Check the command or invocation used to launch the extension.
    CheckCommand,
    /// Check transport configuration (stdio, SSE, HTTP, etc.).
    CheckTransport,
    /// Check the extension server itself (installation, health, version).
    CheckServer,
    /// Check skill frontmatter syntax/schema.
    CheckSkillFrontmatter,
    /// Restore a missing or corrupted skill file.
    RestoreSkillFile,
    /// Update the skill manifest to match the current workspace.
    UpdateSkillManifest,
}

impl ExtensionLoadRemedyCode {
    /// Canonical snake_case string representation used by the database and JSON contract.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CheckPlaceholder => "check_placeholder",
            Self::CheckCommand => "check_command",
            Self::CheckTransport => "check_transport",
            Self::CheckServer => "check_server",
            Self::CheckSkillFrontmatter => "check_skill_frontmatter",
            Self::RestoreSkillFile => "restore_skill_file",
            Self::UpdateSkillManifest => "update_skill_manifest",
        }
    }
}

/// Sole persisted/serialized V1 record for project extension-load failures.
///
/// All UUID and timestamp fields are encoded as strings to match the JSON contract and
/// the existing `djinn-db` conventions (e.g. `Project`, `SessionRecord`). RFC 3339 UTC
/// strings are expected for the timestamp fields; this module does not enforce formats.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ExtensionLoadDiagnosticV1 {
    /// Schema version for writers. Always `1` for this type.
    pub schema_version: i32,
    /// Diagnostic UUID generated once after normalization and reused by all projections.
    pub diagnostic_id: String,
    /// Owning project UUID.
    pub project_id: String,
    /// Associated task UUID, if any. Omitted for doctor-only probes.
    pub task_id: Option<String>,
    /// Associated session UUID, if any. Omitted for doctor-only probes.
    pub session_id: Option<String>,
    /// UUID grouping one loading pass. Retries within the same attempt share this value.
    pub load_attempt_id: String,
    /// Source category for the failing extension.
    pub source_kind: ExtensionLoadSourceKind,
    /// Configured server name or project-relative skill identifier.
    pub source_key: String,
    /// Lifecycle phase where the failure was observed.
    pub phase: ExtensionLoadPhase,
    /// Severity of the failure.
    pub severity: ExtensionLoadSeverity,
    /// Normalized/redacted untrusted summary text.
    pub summary: String,
    /// Selected remedy code.
    pub remedy_code: ExtensionLoadRemedyCode,
    /// Djinn-authored remedy template selected by `remedy_code`.
    pub remedy: String,
    /// Positive occurrence count; incremented for equivalent retries within one attempt.
    pub occurrence_count: u64,
    /// RFC 3339 UTC timestamp of the first observation within this attempt.
    pub first_seen_at: String,
    /// RFC 3339 UTC timestamp of the most recent observation within this attempt.
    pub last_seen_at: String,
    /// RFC 3339 UTC timestamp when the diagnostic record was created.
    pub created_at: String,
}

impl ExtensionLoadDiagnosticV1 {
    /// The schema version emitted by V1 writers.
    pub fn schema_version() -> i32 {
        EXTENSION_LOAD_DIAGNOSTIC_V1_SCHEMA_VERSION
    }

    /// Construct a V1 diagnostic with `occurrence_count` defaulted to 1.
    ///
    /// Callers must supply their own generated UUIDs and RFC 3339 UTC timestamps.
    /// This constructor enforces the V1 writer policy by pinning `schema_version` to
    /// [`EXTENSION_LOAD_DIAGNOSTIC_V1_SCHEMA_VERSION`].
    #[allow(clippy::too_many_arguments)]
    pub fn new_v1(
        diagnostic_id: String,
        project_id: String,
        task_id: Option<String>,
        session_id: Option<String>,
        load_attempt_id: String,
        source_kind: ExtensionLoadSourceKind,
        source_key: String,
        phase: ExtensionLoadPhase,
        severity: ExtensionLoadSeverity,
        summary: String,
        remedy_code: ExtensionLoadRemedyCode,
        remedy: String,
        first_seen_at: String,
        last_seen_at: String,
        created_at: String,
    ) -> Self {
        Self {
            schema_version: EXTENSION_LOAD_DIAGNOSTIC_V1_SCHEMA_VERSION,
            diagnostic_id,
            project_id,
            task_id,
            session_id,
            load_attempt_id,
            source_kind,
            source_key,
            phase,
            severity,
            summary,
            remedy_code,
            remedy,
            occurrence_count: 1,
            first_seen_at,
            last_seen_at,
            created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_diagnostic() -> ExtensionLoadDiagnosticV1 {
        ExtensionLoadDiagnosticV1::new_v1(
            "019f32e7-06c8-7f11-9ffb-56059029f650".to_string(),
            "019f32e7-06c8-7f11-9ffb-56059029f651".to_string(),
            Some("019f32e7-06c8-7f11-9ffb-56059029f652".to_string()),
            Some("019f32e7-06c8-7f11-9ffb-56059029f653".to_string()),
            "019f32e7-06c8-7f11-9ffb-56059029f654".to_string(),
            ExtensionLoadSourceKind::ProjectMcp,
            "search".to_string(),
            ExtensionLoadPhase::ToolsList,
            ExtensionLoadSeverity::Error,
            "tools/list returned invalid JSON".to_string(),
            ExtensionLoadRemedyCode::CheckServer,
            "Check the MCP server health and restart it.".to_string(),
            "2026-07-13T10:00:00Z".to_string(),
            "2026-07-13T10:00:00Z".to_string(),
            "2026-07-13T10:00:00Z".to_string(),
        )
    }

    #[test]
    fn enum_variants_serialize_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&ExtensionLoadSourceKind::ProjectMcp).unwrap(),
            "\"project_mcp\""
        );
        assert_eq!(
            serde_json::to_string(&ExtensionLoadSourceKind::ProjectSkill).unwrap(),
            "\"project_skill\""
        );
        assert_eq!(
            serde_json::to_string(&ExtensionLoadPhase::PlaceholderResolution).unwrap(),
            "\"placeholder_resolution\""
        );
        assert_eq!(
            serde_json::to_string(&ExtensionLoadPhase::ToolsList).unwrap(),
            "\"tools_list\""
        );
        assert_eq!(
            serde_json::to_string(&ExtensionLoadSeverity::Warning).unwrap(),
            "\"warning\""
        );
        assert_eq!(
            serde_json::to_string(&ExtensionLoadRemedyCode::CheckSkillFrontmatter).unwrap(),
            "\"check_skill_frontmatter\""
        );
    }

    #[test]
    fn enum_variants_round_trip() {
        let variants: &[(ExtensionLoadSourceKind, &str)] = &[
            (ExtensionLoadSourceKind::ProjectMcp, "project_mcp"),
            (ExtensionLoadSourceKind::ProjectSkill, "project_skill"),
        ];
        for (variant, expected) in variants {
            let json = serde_json::to_string(variant).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            let parsed: ExtensionLoadSourceKind = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, *variant);
        }

        let phases: &[(ExtensionLoadPhase, &str)] = &[
            (
                ExtensionLoadPhase::PlaceholderResolution,
                "placeholder_resolution",
            ),
            (ExtensionLoadPhase::ProcessStart, "process_start"),
            (ExtensionLoadPhase::Transport, "transport"),
            (ExtensionLoadPhase::Handshake, "handshake"),
            (ExtensionLoadPhase::ToolsList, "tools_list"),
            (ExtensionLoadPhase::Frontmatter, "frontmatter"),
            (ExtensionLoadPhase::MissingFile, "missing_file"),
            (ExtensionLoadPhase::ManifestDrift, "manifest_drift"),
        ];
        for (variant, expected) in phases {
            let json = serde_json::to_string(variant).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            let parsed: ExtensionLoadPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, *variant);
        }

        let severities: &[(ExtensionLoadSeverity, &str)] = &[
            (ExtensionLoadSeverity::Warning, "warning"),
            (ExtensionLoadSeverity::Error, "error"),
        ];
        for (variant, expected) in severities {
            let json = serde_json::to_string(variant).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            let parsed: ExtensionLoadSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, *variant);
        }

        let remedies: &[(ExtensionLoadRemedyCode, &str)] = &[
            (
                ExtensionLoadRemedyCode::CheckPlaceholder,
                "check_placeholder",
            ),
            (ExtensionLoadRemedyCode::CheckCommand, "check_command"),
            (ExtensionLoadRemedyCode::CheckTransport, "check_transport"),
            (ExtensionLoadRemedyCode::CheckServer, "check_server"),
            (
                ExtensionLoadRemedyCode::CheckSkillFrontmatter,
                "check_skill_frontmatter",
            ),
            (
                ExtensionLoadRemedyCode::RestoreSkillFile,
                "restore_skill_file",
            ),
            (
                ExtensionLoadRemedyCode::UpdateSkillManifest,
                "update_skill_manifest",
            ),
        ];
        for (variant, expected) in remedies {
            let json = serde_json::to_string(variant).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            let parsed: ExtensionLoadRemedyCode = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, *variant);
        }
    }

    #[test]
    fn diagnostic_serializes_with_snake_case_fields() {
        let diag = example_diagnostic();
        let json = serde_json::to_value(&diag).unwrap();

        assert_eq!(json.get("schema_version").unwrap().as_i64(), Some(1));
        assert_eq!(
            json.get("diagnostic_id").unwrap().as_str(),
            Some("019f32e7-06c8-7f11-9ffb-56059029f650")
        );
        assert_eq!(
            json.get("project_id").unwrap().as_str(),
            Some("019f32e7-06c8-7f11-9ffb-56059029f651")
        );
        assert_eq!(
            json.get("task_id").unwrap().as_str(),
            Some("019f32e7-06c8-7f11-9ffb-56059029f652")
        );
        assert_eq!(
            json.get("session_id").unwrap().as_str(),
            Some("019f32e7-06c8-7f11-9ffb-56059029f653")
        );
        assert_eq!(
            json.get("load_attempt_id").unwrap().as_str(),
            Some("019f32e7-06c8-7f11-9ffb-56059029f654")
        );
        assert_eq!(
            json.get("source_kind").unwrap().as_str(),
            Some("project_mcp")
        );
        assert_eq!(json.get("source_key").unwrap().as_str(), Some("search"));
        assert_eq!(json.get("phase").unwrap().as_str(), Some("tools_list"));
        assert_eq!(json.get("severity").unwrap().as_str(), Some("error"));
        assert_eq!(
            json.get("summary").unwrap().as_str(),
            Some("tools/list returned invalid JSON")
        );
        assert_eq!(
            json.get("remedy_code").unwrap().as_str(),
            Some("check_server")
        );
        assert_eq!(
            json.get("remedy").unwrap().as_str(),
            Some("Check the MCP server health and restart it.")
        );
        assert_eq!(json.get("occurrence_count").unwrap().as_u64(), Some(1));
        assert_eq!(
            json.get("first_seen_at").unwrap().as_str(),
            Some("2026-07-13T10:00:00Z")
        );
        assert_eq!(
            json.get("last_seen_at").unwrap().as_str(),
            Some("2026-07-13T10:00:00Z")
        );
        assert_eq!(
            json.get("created_at").unwrap().as_str(),
            Some("2026-07-13T10:00:00Z")
        );

        // No camelCase or unexpected top-level keys should be emitted.
        assert!(json.get("schemaVersion").is_none());
        assert!(json.get("diagnosticId").is_none());
        assert!(json.get("projectId").is_none());
    }

    #[test]
    fn diagnostic_round_trips() {
        let diag = example_diagnostic();
        let json = serde_json::to_string(&diag).unwrap();
        let parsed: ExtensionLoadDiagnosticV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, diag);
    }

    #[test]
    fn writer_emits_schema_version_1() {
        let diag = example_diagnostic();
        assert_eq!(diag.schema_version, 1);
        assert_eq!(
            ExtensionLoadDiagnosticV1::schema_version(),
            EXTENSION_LOAD_DIAGNOSTIC_V1_SCHEMA_VERSION
        );
        assert_eq!(EXTENSION_LOAD_DIAGNOSTIC_V1_SCHEMA_VERSION, 1);
    }

    #[test]
    fn deserialization_tolerates_unknown_future_fields() {
        let json = r#"{
            "schema_version": 1,
            "diagnostic_id": "019f32e7-06c8-7f11-9ffb-56059029f650",
            "project_id": "019f32e7-06c8-7f11-9ffb-56059029f651",
            "task_id": null,
            "session_id": null,
            "load_attempt_id": "019f32e7-06c8-7f11-9ffb-56059029f654",
            "source_kind": "project_skill",
            "source_key": "my-skill",
            "phase": "frontmatter",
            "severity": "warning",
            "summary": "frontmatter missing required key",
            "remedy_code": "check_skill_frontmatter",
            "remedy": "Check the skill frontmatter schema.",
            "occurrence_count": 3,
            "first_seen_at": "2026-07-13T10:00:00Z",
            "last_seen_at": "2026-07-13T10:05:00Z",
            "created_at": "2026-07-13T10:00:00Z",
            "future_field": "should be ignored",
            "nested_future": { "ignored": true }
        }"#;

        let parsed: ExtensionLoadDiagnosticV1 = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.diagnostic_id, "019f32e7-06c8-7f11-9ffb-56059029f650");
        assert_eq!(parsed.project_id, "019f32e7-06c8-7f11-9ffb-56059029f651");
        assert_eq!(parsed.task_id, None);
        assert_eq!(parsed.session_id, None);
        assert_eq!(parsed.source_kind, ExtensionLoadSourceKind::ProjectSkill);
        assert_eq!(parsed.phase, ExtensionLoadPhase::Frontmatter);
        assert_eq!(parsed.severity, ExtensionLoadSeverity::Warning);
        assert_eq!(parsed.occurrence_count, 3);
    }

    #[test]
    fn all_v1_source_kinds_covered() {
        let expected = ["project_mcp", "project_skill"];
        for s in expected {
            let parsed: ExtensionLoadSourceKind =
                serde_json::from_str(&format!("\"{s}\"")).unwrap();
            assert_eq!(parsed.as_str(), s);
        }
    }

    #[test]
    fn all_v1_phases_covered() {
        let expected = [
            "placeholder_resolution",
            "process_start",
            "transport",
            "handshake",
            "tools_list",
            "frontmatter",
            "missing_file",
            "manifest_drift",
        ];
        for s in expected {
            let parsed: ExtensionLoadPhase = serde_json::from_str(&format!("\"{s}\"")).unwrap();
            assert_eq!(parsed.as_str(), s);
        }
    }

    #[test]
    fn all_v1_severities_covered() {
        let expected = ["warning", "error"];
        for s in expected {
            let parsed: ExtensionLoadSeverity = serde_json::from_str(&format!("\"{s}\"")).unwrap();
            assert_eq!(parsed.as_str(), s);
        }
    }

    #[test]
    fn all_v1_remedy_codes_covered() {
        let expected = [
            "check_placeholder",
            "check_command",
            "check_transport",
            "check_server",
            "check_skill_frontmatter",
            "restore_skill_file",
            "update_skill_manifest",
        ];
        for s in expected {
            let parsed: ExtensionLoadRemedyCode =
                serde_json::from_str(&format!("\"{s}\"")).unwrap();
            assert_eq!(parsed.as_str(), s);
        }
    }
}
