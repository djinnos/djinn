//! Owner-authorized readiness kickoff service.
//!
//! The public request deliberately contains only client-owned routing data. The
//! repository snapshot and native skill pin are resolved from persisted and
//! platform-owned state before the DB materializer is allowed to create work.

use std::fmt;

use async_trait::async_trait;
use djinn_db::{
    Database, MaterializeReadinessKickoff, ProjectCurrentGraph, ProjectRepository,
    ReadinessKickoffMaterialization, ReadinessRepository, RepoGraphGenerationRepository,
    UserRepository,
};

/// The only native skill a readiness kickoff may use.
pub const READINESS_SKILL_NAME: &str = "agent-readiness-guardrails";
/// The immutable native-skill version required by this readiness protocol.
pub const READINESS_SKILL_VERSION: &str = "1.1.0";

/// Client-controlled values accepted by readiness kickoff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessKickoffRequest {
    pub project_id: String,
    pub authenticated_owner_id: String,
    pub idempotency_key: String,
}

/// Failure to prove availability of the platform-owned exact pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadinessSkillPinError {
    Unavailable {
        detail: String,
    },
    WrongPin {
        registered_name: String,
        registered_version: String,
    },
}

impl fmt::Display for ReadinessSkillPinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { detail } => {
                write!(f, "readiness native skill unavailable: {detail}")
            }
            Self::WrongPin {
                registered_name,
                registered_version,
            } => write!(
                f,
                "readiness native skill pin mismatch: registered {registered_name}@{registered_version}"
            ),
        }
    }
}

impl std::error::Error for ReadinessSkillPinError {}

/// Boundary owned by the composition root that can prove an exact native pin.
///
/// Control-plane never receives a skill identity from the caller and does not
/// depend on the agent crate that owns the compiled native-skill registry.
#[async_trait]
pub trait ReadinessSkillPinResolver: Send + Sync {
    async fn resolve_exact(
        &self,
        name: &'static str,
        version: &'static str,
    ) -> Result<(), ReadinessSkillPinError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadinessKickoffError {
    BlankIdempotencyKey,
    ProjectNotFound {
        project_id: String,
    },
    AuthenticatedOwnerNotFound {
        user_id: String,
    },
    UnauthorizedOwner {
        project_id: String,
        user_id: String,
    },
    ImmutableSnapshotUnavailable {
        project_id: String,
    },
    SkillPinUnavailable {
        detail: String,
    },
    SkillPinMismatch {
        registered_name: String,
        registered_version: String,
    },
    Persistence {
        detail: String,
    },
}

impl fmt::Display for ReadinessKickoffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlankIdempotencyKey => {
                write!(f, "readiness kickoff idempotency key must be non-empty")
            }
            Self::ProjectNotFound { project_id } => {
                write!(f, "readiness project not found: {project_id}")
            }
            Self::AuthenticatedOwnerNotFound { user_id } => {
                write!(f, "authenticated readiness owner not found: {user_id}")
            }
            Self::UnauthorizedOwner {
                project_id,
                user_id,
            } => write!(
                f,
                "authenticated user {user_id} does not own project {project_id}"
            ),
            Self::ImmutableSnapshotUnavailable { project_id } => {
                write!(
                    f,
                    "readiness project has no immutable repository snapshot: {project_id}"
                )
            }
            Self::SkillPinUnavailable { detail } => {
                write!(f, "readiness native skill unavailable: {detail}")
            }
            Self::SkillPinMismatch {
                registered_name,
                registered_version,
            } => write!(
                f,
                "readiness native skill pin mismatch: registered {registered_name}@{registered_version}"
            ),
            Self::Persistence { detail } => {
                write!(f, "readiness kickoff persistence failed: {detail}")
            }
        }
    }
}

impl std::error::Error for ReadinessKickoffError {}

/// Performs all authorization and immutable-context resolution before calling
/// c09x's transactional materializer.
#[derive(Clone)]
pub struct ReadinessKickoffService<R> {
    db: Database,
    pin_resolver: R,
}

impl<R> ReadinessKickoffService<R>
where
    R: ReadinessSkillPinResolver,
{
    pub fn new(db: Database, pin_resolver: R) -> Self {
        Self { db, pin_resolver }
    }

    pub async fn kickoff(
        &self,
        request: ReadinessKickoffRequest,
    ) -> Result<ReadinessKickoffMaterialization, ReadinessKickoffError> {
        if request.idempotency_key.trim().is_empty() {
            return Err(ReadinessKickoffError::BlankIdempotencyKey);
        }

        let project = ProjectRepository::new(self.db.clone(), djinn_core::events::EventBus::noop())
            .get(&request.project_id)
            .await
            .map_err(persistence_error)?
            .ok_or_else(|| ReadinessKickoffError::ProjectNotFound {
                project_id: request.project_id.clone(),
            })?;
        let owner = UserRepository::new(self.db.clone())
            .get_by_id(&request.authenticated_owner_id)
            .await
            .map_err(persistence_error)?
            .ok_or_else(|| ReadinessKickoffError::AuthenticatedOwnerNotFound {
                user_id: request.authenticated_owner_id.clone(),
            })?;
        if !project
            .github_owner
            .eq_ignore_ascii_case(&owner.github_login)
        {
            return Err(ReadinessKickoffError::UnauthorizedOwner {
                project_id: request.project_id,
                user_id: owner.id,
            });
        }

        let snapshot = match RepoGraphGenerationRepository::new(self.db.clone())
            .select_project_current_graph(&project.id)
            .await
            .map_err(persistence_error)?
        {
            ProjectCurrentGraph::Current(generation) => generation.commit_sha,
            ProjectCurrentGraph::LegacyFallback(cache) => cache.commit_sha,
            ProjectCurrentGraph::Unavailable => {
                return Err(ReadinessKickoffError::ImmutableSnapshotUnavailable {
                    project_id: project.id,
                });
            }
        };
        if snapshot.trim().is_empty() {
            return Err(ReadinessKickoffError::ImmutableSnapshotUnavailable {
                project_id: project.id,
            });
        }

        self.pin_resolver
            .resolve_exact(READINESS_SKILL_NAME, READINESS_SKILL_VERSION)
            .await
            .map_err(|error| match error {
                ReadinessSkillPinError::Unavailable { detail } => {
                    ReadinessKickoffError::SkillPinUnavailable { detail }
                }
                ReadinessSkillPinError::WrongPin {
                    registered_name,
                    registered_version,
                } => ReadinessKickoffError::SkillPinMismatch {
                    registered_name,
                    registered_version,
                },
            })?;

        ReadinessRepository::new(self.db.clone())
            .materialize_kickoff(MaterializeReadinessKickoff {
                project_id: project.id,
                creator_user_id: owner.id,
                idempotency_key: request.idempotency_key,
                repository_snapshot: snapshot,
                skill_name: READINESS_SKILL_NAME.into(),
                skill_version: READINESS_SKILL_VERSION.into(),
            })
            .await
            .map_err(persistence_error)
    }
}

fn persistence_error(error: impl fmt::Display) -> ReadinessKickoffError {
    ReadinessKickoffError::Persistence {
        detail: error.to_string(),
    }
}
