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

pub use database::{
    Database, DatabaseBackendKind, DatabaseBootstrapInfo, DatabaseConnectConfig,
    MysqlBackendFlavor, MysqlDatabaseConfig, SqliteDatabaseConfig, SqliteVecStatus,
    default_db_path,
};
pub use error::{DbError as Error, DbResult as Result};
pub use migrations::{
    mysql_notes_fulltext_prototype, mysql_schema_snapshot, sqlite_schema_snapshot,
};
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
        LexicalSearchMode, LexicalSearchPlan, NoopNoteVectorStore, NoteAssociationEntry,
        NoteConsolidationRepository, NoteDedupCandidate, NoteEmbeddingMatch, NoteEmbeddingProvider,
        NoteEmbeddingRecord, NoteRepository, NoteSearchParams, NoteVectorBackend, NoteVectorStore,
        QdrantNoteVectorStore, STALE_CITATION, SqliteVecNoteVectorStore, UpdateNoteIndexParams,
        UpsertNoteEmbedding, build_lexical_search_plan, file_path_for, file_path_for_with_status,
        folder_for_type, folder_for_type_with_status, infer_note_type, is_singleton,
        normalize_virtual_note_path, permalink_for, permalink_for_with_status,
        permalink_from_virtual_note_path, render_note_markdown, sanitize_mysql_boolean_query,
        sanitize_sqlite_fts5_query, slugify, title_from_permalink,
        validate_mysql_fulltext_threshold, virtual_note_path_for_permalink,
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
