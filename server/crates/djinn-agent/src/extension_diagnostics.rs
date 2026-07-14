//! Trusted normalization and persistence for project extension-load failures.
//!
//! Detectors supply only bounded facts. This module is deliberately the sole
//! boundary between untrusted extension text and the canonical V1 repository.

// The MCP, skill, session, and doctor call sites are deliberately introduced
// independently. Keep this shared crate-private boundary available before the
// first caller lands without weakening the crate-wide `dead_code` policy.
#![allow(
    dead_code,
    reason = "extension diagnostic detector integrations consume this shared boundary incrementally"
)]

use std::sync::LazyLock;

use djinn_core::extension_diagnostics::{
    ExtensionLoadDiagnosticV1, ExtensionLoadPhase, ExtensionLoadRemedyCode, ExtensionLoadSeverity,
    ExtensionLoadSourceKind,
};
use djinn_db::{ExtensionLoadDiagnosticRepository, InsertExtensionLoadDiagnostic};
use regex::{Captures, Regex};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const MAX_SUMMARY_BYTES: usize = 512;
const TRUNCATION_SUFFIX: &str = "…[truncated]";
const REDACTED: &str = "[redacted]";

static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b([a-z][a-z0-9+.-]*://)([^/\s@?#]+@)?([^/\s?#]+)([^\s?#]*)(?:\?[^\s#]*)?(#[^\s]*)?",
    )
    .expect("valid URL redaction expression")
});
static AUTHORIZATION_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)\b(authorization|proxy-authorization)\s*:\s*[^\r\n]*")
        .expect("valid authorization header expression")
});
static CREDENTIAL_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b((?:[a-z_][a-z0-9_-]*)?(?:password|passwd|pwd|secret|token|api[_-]?key|credential|authorization)[a-z0-9_-]*)\s*[:=]\s*(?:"[^"]*"|'[^']*'|[^\s,;]+)"#)
        .expect("valid credential assignment expression")
});
static BEARER_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bbearer\s+[a-z0-9._~+/-]+").expect("valid bearer token expression")
});
static SECRET_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{36,}|glpat-[A-Za-z0-9_-]{20,}|xox[baprs]-[A-Za-z0-9-]{10,}|sk-[A-Za-z0-9_-]{20,}|(?:sk|pk|rk)_(?:live|test)_[A-Za-z0-9]{20,}|eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}|-----BEGIN (?:RSA |EC |OPENSSH |DSA |)?PRIVATE KEY-----)")
        .expect("valid secret detector expression")
});
static UNIX_ABSOLUTE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)(^|[\s(="'`:])/(?:[^\s\])}"'`,;]+)"#)
        .expect("valid Unix absolute path expression")
});
static WINDOWS_ABSOLUTE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?im)(^|[\s(="'`:])(?:[a-z]:\x5c|\x5c\x5c)[^\s\])}"'`,;]+"#)
        .expect("valid Windows absolute path expression")
});
/// Minimum detector-owned fact accepted at the untrusted boundary.
///
/// `summary_material` must be a short detector-selected description. Detectors
/// must not pass raw stderr, commands, environment dumps, raw frontmatter, or
/// remedy prose: those are intentionally absent from this type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExtensionDiagnosticFact {
    pub source_kind: ExtensionLoadSourceKind,
    pub source_key: String,
    pub phase: ExtensionLoadPhase,
    pub severity: ExtensionLoadSeverity,
    pub remedy_code: ExtensionLoadRemedyCode,
    pub summary_material: String,
}

/// Associations that scope one observation to a project load attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExtensionDiagnosticAssociations {
    pub project_id: String,
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    pub load_attempt_id: String,
}

/// Normalized data safe to fingerprint and hand to the persistence repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NormalizedExtensionDiagnostic {
    pub source_key: String,
    pub summary: String,
    pub summary_fingerprint: String,
    pub remedy: &'static str,
}

/// Normalize one fact before it can be fingerprinted or persisted.
pub(crate) fn normalize_extension_diagnostic(
    fact: &ExtensionDiagnosticFact,
) -> NormalizedExtensionDiagnostic {
    let summary = normalize_untrusted_text(&fact.summary_material);
    NormalizedExtensionDiagnostic {
        source_key: normalize_untrusted_text(&fact.source_key),
        summary_fingerprint: summary_fingerprint(&summary),
        summary,
        remedy: remedy_template(fact.remedy_code),
    }
}

/// Build canonical repository input. This is intentionally separate to make
/// the normalization-before-fingerprinting boundary testable by all detectors.
pub(crate) fn build_insert_extension_diagnostic(
    associations: ExtensionDiagnosticAssociations,
    fact: &ExtensionDiagnosticFact,
) -> InsertExtensionLoadDiagnostic {
    let normalized = normalize_extension_diagnostic(fact);
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("UTC time always formats as RFC 3339");
    InsertExtensionLoadDiagnostic {
        project_id: associations.project_id,
        task_id: associations.task_id,
        session_id: associations.session_id,
        load_attempt_id: associations.load_attempt_id,
        source_kind: fact.source_kind,
        source_key: normalized.source_key,
        phase: fact.phase,
        severity: fact.severity,
        summary: normalized.summary,
        summary_fingerprint: normalized.summary_fingerprint,
        remedy_code: fact.remedy_code,
        remedy: normalized.remedy.to_owned(),
        first_seen_at: now.clone(),
        last_seen_at: now.clone(),
        created_at: now,
    }
}

/// Persist one observation through the canonical repository's atomic dedupe.
pub(crate) async fn persist_extension_diagnostic(
    repository: &ExtensionLoadDiagnosticRepository,
    associations: ExtensionDiagnosticAssociations,
    fact: ExtensionDiagnosticFact,
) -> djinn_db::Result<ExtensionLoadDiagnosticV1> {
    repository
        .insert_or_increment(build_insert_extension_diagnostic(associations, &fact))
        .await
}

pub(crate) async fn persist_extension_diagnostic_batch(
    repository: &ExtensionLoadDiagnosticRepository,
    associations: ExtensionDiagnosticAssociations,
    facts: Vec<ExtensionDiagnosticFact>,
) -> djinn_db::Result<Vec<ExtensionLoadDiagnosticV1>> {
    for fact in facts {
        persist_extension_diagnostic(repository, associations.clone(), fact).await?;
    }
    repository
        .list_for_load_attempt(&associations.project_id, &associations.load_attempt_id)
        .await
}

/// Djinn-owned prose. Extension and detector text never selects this value.
pub(crate) fn remedy_template(code: ExtensionLoadRemedyCode) -> &'static str {
    match code {
        ExtensionLoadRemedyCode::CheckPlaceholder => "Check the configured placeholder value.",
        ExtensionLoadRemedyCode::CheckCommand => "Check the configured extension command.",
        ExtensionLoadRemedyCode::CheckTransport => "Check the extension transport configuration.",
        ExtensionLoadRemedyCode::CheckServer => {
            "Check the extension server installation and health."
        }
        ExtensionLoadRemedyCode::CheckSkillFrontmatter => {
            "Check the skill frontmatter syntax and fields."
        }
        ExtensionLoadRemedyCode::RestoreSkillFile => "Restore the declared skill file.",
        ExtensionLoadRemedyCode::UpdateSkillManifest => {
            "Update the skill manifest to match the workspace."
        }
    }
}

fn normalize_untrusted_text(input: &str) -> String {
    let without_urls = URL.replace_all(input, |captures: &Captures<'_>| {
        format!("{}{}{}", &captures[1], REDACTED, &captures[3])
    });
    let without_headers = AUTHORIZATION_HEADER
        .replace_all(&without_urls, |captures: &Captures<'_>| {
            format!("{}: {REDACTED}", &captures[1])
        });
    let without_assignments = CREDENTIAL_ASSIGNMENT
        .replace_all(&without_headers, |captures: &Captures<'_>| {
            format!("{}={REDACTED}", &captures[1])
        });
    let without_bearers =
        BEARER_TOKEN.replace_all(&without_assignments, format!("Bearer {REDACTED}"));
    let without_secrets = SECRET_TOKEN.replace_all(&without_bearers, REDACTED);
    let without_unix_paths = UNIX_ABSOLUTE_PATH
        .replace_all(&without_secrets, |captures: &Captures<'_>| {
            format!("{}[path redacted]", &captures[1])
        });
    let without_paths = WINDOWS_ABSOLUTE_PATH
        .replace_all(&without_unix_paths, |captures: &Captures<'_>| {
            format!("{}[path redacted]", &captures[1])
        });

    let controls_removed: String = without_paths
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect();
    let whitespace_normalized = controls_removed
        .lines()
        .filter_map(|line| {
            let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
            (!normalized.is_empty()).then_some(normalized)
        })
        .collect::<Vec<_>>()
        .join("\n");
    truncate_utf8(&escape_renderer_delimiters(&whitespace_normalized))
}

fn escape_renderer_delimiters(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(
            character,
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '<' | '>' | '#' | '|' | '~'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn truncate_utf8(text: &str) -> String {
    if text.len() <= MAX_SUMMARY_BYTES {
        return text.to_owned();
    }
    let maximum_prefix = MAX_SUMMARY_BYTES - TRUNCATION_SUFFIX.len();
    let mut boundary = maximum_prefix;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}{TRUNCATION_SUFFIX}", &text[..boundary])
}

fn summary_fingerprint(summary: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(summary.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_db::{
        CreateSessionParams, Database, ProjectRepository, SessionRepository, TaskRepository,
    };

    fn fact(summary_material: impl Into<String>) -> ExtensionDiagnosticFact {
        ExtensionDiagnosticFact {
            source_kind: ExtensionLoadSourceKind::ProjectMcp,
            source_key: "search".to_owned(),
            phase: ExtensionLoadPhase::ToolsList,
            severity: ExtensionLoadSeverity::Error,
            remedy_code: ExtensionLoadRemedyCode::CheckServer,
            summary_material: summary_material.into(),
        }
    }

    #[test]
    fn malicious_fixture_is_redacted_before_fingerprinting() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/extension_diagnostics/malicious_and_oversized.json"
        ))
        .unwrap();
        let normalized =
            normalize_extension_diagnostic(&fact(fixture["summary_material"].as_str().unwrap()));
        for leaked in [
            "supersecret",
            "AKIA1234567890ABCDEF",
            "ghp_abcdefghijklmnopqrstuvwxyz1234567890AB",
            "/home/alice",
            "C:\\Users\\Alice",
        ] {
            assert!(!normalized.summary.contains(leaked), "leaked {leaked}");
            assert!(!normalized.summary_fingerprint.contains(leaked));
        }
        assert!(normalized.summary.len() <= MAX_SUMMARY_BYTES);
        assert!(normalized.summary.ends_with(TRUNCATION_SUFFIX));
        assert!(!normalized.summary.contains('\r'));
        assert!(!normalized.summary.contains('\u{1b}'));
        assert!(normalized.summary.contains("\\{evil\\}"));
    }

    #[test]
    fn punctuation_adjacent_absolute_paths_are_redacted() {
        assert_eq!(
            normalize_untrusted_text("load failed:/home/alice/.config/token"),
            r"load failed:\[path redacted\]"
        );
        assert_eq!(
            normalize_untrusted_text(r"load failed:C:\Users\Alice\token"),
            r"load failed:\[path redacted\]"
        );
    }

    #[test]
    fn malicious_fixture_is_redacted_at_the_repository_input_boundary() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/extension_diagnostics/malicious_and_oversized.json"
        ))
        .unwrap();
        let insert = build_insert_extension_diagnostic(
            ExtensionDiagnosticAssociations {
                project_id: "project".to_owned(),
                task_id: Some("task".to_owned()),
                session_id: Some("session".to_owned()),
                load_attempt_id: "attempt".to_owned(),
            },
            &fact(fixture["summary_material"].as_str().unwrap()),
        );

        for leaked in [
            "supersecret",
            "AKIA1234567890ABCDEF",
            "ghp_abcdefghijklmnopqrstuvwxyz1234567890AB",
            "/home/alice",
            "C:\\Users\\Alice",
        ] {
            assert!(!insert.summary.contains(leaked), "persisted {leaked}");
            assert!(!insert.summary_fingerprint.contains(leaked));
        }
        assert_eq!(
            insert.summary_fingerprint,
            summary_fingerprint(&insert.summary)
        );
    }

    #[test]
    fn equivalent_normalized_observations_share_a_fingerprint() {
        let first =
            normalize_extension_diagnostic(&fact("failed at /home/alice/project with token=one"));
        let second =
            normalize_extension_diagnostic(&fact("failed  at /srv/build/project with token=two"));
        assert_eq!(first.summary, second.summary);
        assert_eq!(first.summary_fingerprint, second.summary_fingerprint);
    }

    #[test]
    fn canonical_inputs_keep_source_and_phase_distinct_and_remedy_is_trusted() {
        let associations = ExtensionDiagnosticAssociations {
            project_id: "project".to_owned(),
            task_id: None,
            session_id: None,
            load_attempt_id: "attempt".to_owned(),
        };
        let first = build_insert_extension_diagnostic(associations.clone(), &fact("bad"));
        let mut second_fact = fact("bad");
        second_fact.source_kind = ExtensionLoadSourceKind::ProjectSkill;
        second_fact.phase = ExtensionLoadPhase::Frontmatter;
        second_fact.remedy_code = ExtensionLoadRemedyCode::CheckSkillFrontmatter;
        let second = build_insert_extension_diagnostic(associations, &second_fact);
        assert_eq!(first.summary_fingerprint, second.summary_fingerprint);
        assert_ne!(first.source_kind, second.source_kind);
        assert_ne!(first.phase, second.phase);
        assert_eq!(
            second.remedy,
            remedy_template(ExtensionLoadRemedyCode::CheckSkillFrontmatter)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_correlation_fixture_persists_two_sources_retries_and_attempts() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/extension_diagnostics/session_two_sources.json"
        ))
        .unwrap();
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let project = ProjectRepository::new(db.clone(), events.clone())
            .create_with_id(
                fixture["project_id"].as_str().unwrap(),
                "extension-diagnostics-correlation",
                "test",
                "extension-diagnostics-correlation",
            )
            .await
            .unwrap();
        let task = TaskRepository::new(db.clone(), events.clone())
            .create_in_project(
                &project.id,
                None,
                "diagnostic correlation",
                "",
                "",
                "task",
                1,
                "",
                Some("open"),
                None,
            )
            .await
            .unwrap();
        let session = SessionRepository::new(db.clone(), events)
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&task.id),
                model: "test-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        let associations = ExtensionDiagnosticAssociations {
            project_id: project.id.clone(),
            task_id: Some(task.id.clone()),
            session_id: Some(session.id.clone()),
            load_attempt_id: fixture["first_attempt_id"].as_str().unwrap().to_owned(),
        };
        let mcp = ExtensionDiagnosticFact {
            source_kind: ExtensionLoadSourceKind::ProjectMcp,
            source_key: fixture["facts"][0]["source_key"]
                .as_str()
                .unwrap()
                .to_owned(),
            phase: ExtensionLoadPhase::ToolsList,
            severity: ExtensionLoadSeverity::Error,
            remedy_code: ExtensionLoadRemedyCode::CheckServer,
            summary_material: fixture["facts"][0]["summary_material"]
                .as_str()
                .unwrap()
                .to_owned(),
        };
        let skill = ExtensionDiagnosticFact {
            source_kind: ExtensionLoadSourceKind::ProjectSkill,
            source_key: fixture["facts"][1]["source_key"]
                .as_str()
                .unwrap()
                .to_owned(),
            phase: ExtensionLoadPhase::Frontmatter,
            severity: ExtensionLoadSeverity::Warning,
            remedy_code: ExtensionLoadRemedyCode::CheckSkillFrontmatter,
            summary_material: fixture["facts"][1]["summary_material"]
                .as_str()
                .unwrap()
                .to_owned(),
        };
        let repository = ExtensionLoadDiagnosticRepository::new(db);
        let first = persist_extension_diagnostic(&repository, associations.clone(), mcp.clone())
            .await
            .unwrap();
        let retry = persist_extension_diagnostic(&repository, associations.clone(), mcp.clone())
            .await
            .unwrap();
        let second_source = persist_extension_diagnostic(&repository, associations.clone(), skill)
            .await
            .unwrap();
        let canonical = repository
            .list_for_load_attempt(&project.id, &associations.load_attempt_id)
            .await
            .unwrap();
        let later = persist_extension_diagnostic(
            &repository,
            ExtensionDiagnosticAssociations {
                load_attempt_id: fixture["later_attempt_id"].as_str().unwrap().to_owned(),
                ..associations.clone()
            },
            mcp,
        )
        .await
        .unwrap();
        assert_eq!(first.project_id, project.id);
        assert_eq!(first.task_id.as_deref(), Some(task.id.as_str()));
        assert_eq!(first.session_id.as_deref(), Some(session.id.as_str()));
        assert_eq!(first.diagnostic_id, retry.diagnostic_id);
        assert_eq!(retry.occurrence_count, 2);
        assert_eq!(canonical.len(), 2);
        assert_eq!(canonical[0].diagnostic_id, first.diagnostic_id);
        assert_eq!(canonical[0].occurrence_count, 2);
        assert_eq!(canonical[1].diagnostic_id, second_source.diagnostic_id);
        assert_ne!(first.source_kind, second_source.source_kind);
        assert_ne!(first.load_attempt_id, later.load_attempt_id);
        assert_ne!(first.diagnostic_id, later.diagnostic_id);
    }
}
