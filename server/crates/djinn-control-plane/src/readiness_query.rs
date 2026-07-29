//! Owner-authorized readiness query service.
//!
//! This boundary deliberately exposes consumer DTOs rather than database row
//! types. Readiness projections remain owned by `ReadinessRepository`.

use std::fmt;

use djinn_db::{
    Database, ProjectRepository, ReadinessRepository, ReadinessRunDetail, ReadinessRunRow,
    UserRepository,
};
use serde::{Deserialize, Serialize};

/// Authenticated request for the active readiness run, or the latest terminal run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessProjectQuery {
    pub project_id: String,
    pub authenticated_owner_id: String,
}

/// Authenticated request for one readiness run scoped to its containing project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessRunQuery {
    pub project_id: String,
    pub run_id: String,
    pub authenticated_owner_id: String,
}

/// Stable readiness run summary for consumer surfaces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadinessRunSummary {
    pub id: String,
    pub project_id: String,
    pub status: String,
    pub repository_snapshot: String,
    pub skill_name: String,
    pub skill_version: String,
    pub expected_area_count: Option<i32>,
    pub created_at: String,
    pub completed_at: Option<String>,
}

/// Stable full readiness projection for consumer surfaces.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadinessRunDetailDto {
    pub run: ReadinessRunSummary,
    pub areas: Vec<ReadinessAreaDto>,
    pub area_scores: Vec<ReadinessAreaScoreDto>,
    pub project_score: Option<ReadinessProjectScoreDto>,
    pub suggestions: Vec<ReadinessSuggestionDto>,
    pub events: Vec<ReadinessEventDto>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadinessAreaDto {
    pub id: String,
    pub area_key: String,
    pub composition: serde_json::Value,
    pub path_scopes: serde_json::Value,
    pub frozen_at: String,
    pub status: String,
    pub attempts: Vec<ReadinessAttemptDto>,
    pub accepted_findings: Vec<ReadinessFindingDto>,
    pub accepted_outputs: Vec<ReadinessOutputDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadinessAttemptDto {
    pub id: String,
    pub attempt_number: i32,
    pub status: String,
    pub payload_digest: Option<String>,
    pub created_at: String,
    pub terminal_at: Option<String>,
    pub is_current: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadinessFindingDto {
    pub id: String,
    pub attempt_id: String,
    pub guardrail_key: String,
    pub status: String,
    pub severity: String,
    pub confidence: f64,
    pub evidence: serde_json::Value,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadinessOutputDto {
    pub attempt_id: String,
    pub result: serde_json::Value,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadinessAreaScoreDto {
    pub area_id: String,
    pub score: f64,
    pub applicable_weight: i32,
    pub covered_weight: f64,
    pub status: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadinessProjectScoreDto {
    pub score: f64,
    pub band: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadinessSuggestionDto {
    pub id: String,
    pub dedupe_key: String,
    pub suggestion: serde_json::Value,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadinessEventDto {
    pub id: String,
    pub event_kind: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadinessQueryError {
    ProjectNotFound { project_id: String },
    AuthenticatedOwnerNotFound { user_id: String },
    UnauthorizedOwner { project_id: String, user_id: String },
    RunNotFound { run_id: String },
    RunProjectMismatch {
        project_id: String,
        run_id: String,
        actual_project_id: String,
    },
    Persistence { detail: String },
}

impl fmt::Display for ReadinessQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectNotFound { project_id } => write!(f, "readiness project not found: {project_id}"),
            Self::AuthenticatedOwnerNotFound { user_id } => {
                write!(f, "authenticated readiness owner not found: {user_id}")
            }
            Self::UnauthorizedOwner { project_id, user_id } => {
                write!(f, "authenticated user {user_id} does not own project {project_id}")
            }
            Self::RunNotFound { run_id } => write!(f, "readiness run not found: {run_id}"),
            Self::RunProjectMismatch { project_id, run_id, actual_project_id } => write!(
                f,
                "readiness run {run_id} belongs to project {actual_project_id}, not {project_id}"
            ),
            Self::Persistence { detail } => write!(f, "readiness query persistence failed: {detail}"),
        }
    }
}

impl std::error::Error for ReadinessQueryError {}

/// Performs ownership verification before delegating all readiness reads to the repository.
#[derive(Clone)]
pub struct ReadinessQueryService {
    db: Database,
}

impl ReadinessQueryService {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn active_or_latest(
        &self,
        query: ReadinessProjectQuery,
    ) -> Result<Option<ReadinessRunSummary>, ReadinessQueryError> {
        self.authorize(&query.project_id, &query.authenticated_owner_id).await?;
        ReadinessRepository::new(self.db.clone())
            .active_or_latest_for_project(&query.project_id)
            .await
            .map(|run| run.map(Into::into))
            .map_err(persistence_error)
    }

    pub async fn run_detail(
        &self,
        query: ReadinessRunQuery,
    ) -> Result<ReadinessRunDetailDto, ReadinessQueryError> {
        self.authorize(&query.project_id, &query.authenticated_owner_id).await?;
        let detail = ReadinessRepository::new(self.db.clone())
            .run_detail(&query.run_id)
            .await
            .map_err(persistence_error)?
            .ok_or_else(|| ReadinessQueryError::RunNotFound {
                run_id: query.run_id.clone(),
            })?;
        if detail.run.project_id != query.project_id {
            return Err(ReadinessQueryError::RunProjectMismatch {
                project_id: query.project_id,
                run_id: query.run_id,
                actual_project_id: detail.run.project_id,
            });
        }
        Ok(detail.into())
    }

    async fn authorize(&self, project_id: &str, authenticated_owner_id: &str) -> Result<(), ReadinessQueryError> {
        let project = ProjectRepository::new(self.db.clone(), djinn_core::events::EventBus::noop())
            .get(project_id)
            .await
            .map_err(persistence_error)?
            .ok_or_else(|| ReadinessQueryError::ProjectNotFound { project_id: project_id.into() })?;
        let owner = UserRepository::new(self.db.clone())
            .get_by_id(authenticated_owner_id)
            .await
            .map_err(persistence_error)?
            .ok_or_else(|| ReadinessQueryError::AuthenticatedOwnerNotFound {
                user_id: authenticated_owner_id.into(),
            })?;
        if project.github_owner.eq_ignore_ascii_case(&owner.github_login) {
            Ok(())
        } else {
            Err(ReadinessQueryError::UnauthorizedOwner {
                project_id: project.id,
                user_id: owner.id,
            })
        }
    }
}

impl From<ReadinessRunRow> for ReadinessRunSummary {
    fn from(run: ReadinessRunRow) -> Self {
        Self {
            id: run.id,
            project_id: run.project_id,
            status: run.status,
            repository_snapshot: run.repository_snapshot,
            skill_name: run.skill_name,
            skill_version: run.skill_version,
            expected_area_count: run.expected_area_count,
            created_at: run.created_at,
            completed_at: run.completed_at,
        }
    }
}

impl From<ReadinessRunDetail> for ReadinessRunDetailDto {
    fn from(detail: ReadinessRunDetail) -> Self {
        Self {
            run: detail.run.into(),
            areas: detail.areas.into_iter().map(|area| ReadinessAreaDto {
                id: area.area.id,
                area_key: area.area.area_key,
                composition: area.area.composition,
                path_scopes: area.area.path_scopes,
                frozen_at: area.area.frozen_at,
                status: area.area.status,
                attempts: area.attempts.into_iter().map(|attempt| ReadinessAttemptDto {
                    id: attempt.attempt.id,
                    attempt_number: attempt.attempt.attempt_number,
                    status: attempt.attempt.status,
                    payload_digest: attempt.attempt.payload_digest,
                    created_at: attempt.attempt.created_at,
                    terminal_at: attempt.attempt.terminal_at,
                    is_current: attempt.is_current,
                }).collect(),
                accepted_findings: area.accepted_findings.into_iter().map(|finding| ReadinessFindingDto {
                    id: finding.id,
                    attempt_id: finding.attempt_id,
                    guardrail_key: finding.guardrail_key,
                    status: finding.status,
                    severity: finding.severity,
                    confidence: finding.confidence,
                    evidence: finding.evidence,
                    created_at: finding.created_at,
                }).collect(),
                accepted_outputs: area.accepted_outputs.into_iter().map(|output| ReadinessOutputDto {
                    attempt_id: output.attempt_id,
                    result: output.result,
                    created_at: output.created_at,
                }).collect(),
            }).collect(),
            area_scores: detail.area_scores.into_iter().map(|score| ReadinessAreaScoreDto {
                area_id: score.area_id,
                score: score.score,
                applicable_weight: score.applicable_weight,
                covered_weight: score.covered_weight,
                status: score.status,
                created_at: score.created_at,
            }).collect(),
            project_score: detail.project_score.map(|score| ReadinessProjectScoreDto {
                score: score.score,
                band: score.band,
                created_at: score.created_at,
            }),
            suggestions: detail.suggestions.into_iter().map(|suggestion| ReadinessSuggestionDto {
                id: suggestion.id,
                dedupe_key: suggestion.dedupe_key,
                suggestion: suggestion.suggestion,
                created_at: suggestion.created_at,
            }).collect(),
            events: detail.events.into_iter().map(|event| ReadinessEventDto {
                id: event.id,
                event_kind: event.event_kind,
                payload: event.payload,
                created_at: event.created_at,
            }).collect(),
        }
    }
}

fn persistence_error(error: impl fmt::Display) -> ReadinessQueryError {
    ReadinessQueryError::Persistence { detail: error.to_string() }
}
