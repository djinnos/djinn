pub mod crypto;
pub mod database;
pub mod error;
pub mod migrations;
pub mod note_hash;
pub mod repositories;

pub mod test_support {
    pub use crate::repositories::test_support::{
        HousekeepingFixture, HousekeepingFixtureExpectedCounts, HousekeepingFixtureProject,
        build_multi_project_housekeeping_fixture, event_bus_for, make_project,
    };
}

pub use database::{Database, SqliteVecStatus, default_db_path};
pub use error::{DbError as Error, DbResult as Result};
pub use repositories::{
    agent::{
        AgentCreateInput, AgentListQuery, AgentListResult, AgentMetrics, AgentRepository,
        AgentUpdateInput, LearnedPromptHistoryEntry, PendingAmendmentEvaluation, VALID_BASE_ROLES,
        WindowedRoleMetrics,
    },
    epic::{
        EpicCountQuery, EpicCreateInput, EpicListQuery, EpicListResult, EpicRepository,
        EpicTaskCounts, EpicUpdateInput,
    },
    events::EventsRepository,
    git_settings::GitSettingsRepository,
    models::ModelsRepository,
    note::{
        CONTRADICTION, ConsolidatedNoteProvenance, ConsolidationCandidateEdge,
        ConsolidationCluster, ConsolidationNote, ConsolidationRunMetric, ContradictionCandidate,
        CreateCanonicalConsolidatedNote, CreateConsolidationRunMetric,
        CreatedCanonicalConsolidatedNote, DbNoteGroup, EmbeddedNote, LexicalSearchBackend,
        LexicalSearchMode, LexicalSearchPlan, NoteAssociationEntry, NoteConsolidationRepository,
        NoteDedupCandidate, NoteEmbeddingMatch, NoteEmbeddingProvider, NoteEmbeddingRecord,
        NoteRepository, NoteSearchParams, STALE_CITATION, UpdateNoteIndexParams,
        UpsertNoteEmbedding, build_lexical_search_plan, file_path_for, file_path_for_with_status,
        folder_for_type, folder_for_type_with_status, is_singleton, permalink_for,
        permalink_for_with_status, sanitize_mysql_boolean_query, sanitize_sqlite_fts5_query,
        slugify, validate_mysql_fulltext_threshold,
    },
    project::{ProjectConfig, ProjectRepository, VerificationRule, validate_verification_rules},
    repo_graph_cache::{CachedRepoGraph, RepoGraphCacheInsert, RepoGraphCacheRepository},
    repo_map_cache::{CachedRepoMap, RepoMapCacheInsert, RepoMapCacheKey, RepoMapCacheRepository},
    session::{CreateSessionParams, SessionRepository},
    session_message::SessionMessageRepository,
    settings::SettingsRepository,
    task::TaskRepository,
    task::{
        ActivityQuery, BlockerRef, CountQuery, CreateTaskInProjectParams, CreateTaskParams,
        ListQuery, ListResult, ReadyQuery, UpdateTaskParams,
    },
    verification_cache::{CachedVerification, VerificationCacheRepository},
    verification_result::{
        VerificationResultRepository, VerificationStepInsert, VerificationStepRow,
    },
};
