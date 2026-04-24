pub mod background;
pub mod crypto;
pub mod database;
pub mod error;
pub mod migrations;
pub mod note_hash;
pub mod repositories;
pub mod retry;

pub mod test_support {
    pub use crate::repositories::test_support::{
        HousekeepingFixture, HousekeepingFixtureExpectedCounts, HousekeepingFixtureProject,
        build_multi_project_housekeeping_fixture, event_bus_for, make_project,
    };
}

pub use database::{
    Database, DatabaseBackendCapabilities, DatabaseBackendKind, DatabaseBootstrapInfo,
    DatabaseConnectConfig, MysqlBackendFlavor, MysqlDatabaseConfig, NoteSearchBackend,
    NoteVectorBackend, SqliteVecStatus, default_db_path,
};
pub use error::{DbError as Error, DbResult as Result};
pub use repositories::{
    agent::{
        AgentCreateInput, AgentListQuery, AgentListResult, AgentMetrics, AgentRepository,
        AgentUpdateInput, LearnedPromptHistoryEntry, PendingAmendmentEvaluation, VALID_BASE_ROLES,
        WindowedRoleMetrics,
    },
    commit_file_changes::{
        CommitFileChange, CommitFileChangeRepository, CoupledFile, FileChurn,
    },
    dolt_branch::{
        DoltBranchError, DoltBranchLifecycle, DoltBranchLifecycleAction, DoltBranchLifecycleResult,
        DoltBranchSqlHelper,
    },
    dolt_history_maintenance::{
        DoltHistoryMaintenanceAction, DoltHistoryMaintenanceError, DoltHistoryMaintenanceExecution,
        DoltHistoryMaintenancePlan, DoltHistoryMaintenancePolicy, DoltHistoryMaintenanceReport,
        DoltHistoryMaintenanceService, DoltHistoryMaintenanceSnapshot, DoltHistoryTableCount,
        plan_dolt_history_maintenance, verify_row_counts,
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
        NoteEmbeddingRecord, NoteRepository, NoteSearchParams, NoteVectorStore, QdrantConfig,
        QdrantNoteVectorStore, STALE_CITATION, UpdateNoteIndexParams, UpsertNoteEmbedding,
        build_lexical_search_plan, executable_lexical_search_sql, file_path_for,
        file_path_for_with_status, folder_for_type, folder_for_type_with_status,
        infer_embedding_branch_from_worktree, infer_note_type, is_singleton,
        lexical_search_threshold, normalize_lexical_score, normalize_virtual_note_path,
        permalink_for, permalink_for_with_status, permalink_from_virtual_note_path,
        render_note_markdown, sanitize_mysql_boolean_query, sanitize_sqlite_fts5_query, slugify,
        task_branch_name, title_from_permalink, validate_mysql_fulltext_threshold,
        virtual_note_path_for_permalink,
    },
    org_config::{NewOrgConfig, OrgConfig, OrgConfigRepository},
    project::{
        ProjectConfig, ProjectDispatchReadiness, ProjectImage, ProjectImageStatus,
        ProjectRepository,
    },
    repo_graph_cache::{CachedRepoGraph, RepoGraphCacheInsert, RepoGraphCacheRepository},
    session::{CreateSessionParams, SessionRepository},
    session_auth::{CreateUserAuthSession, SessionAuthRepository, UserAuthSessionRecord},
    session_message::SessionMessageRepository,
    settings::SettingsRepository,
    task::TaskRepository,
    task::{
        ActivityQuery, BlockerRef, CountQuery, CreateTaskInProjectParams, CreateTaskParams,
        ListQuery, ListResult, ReadyQuery, UpdateTaskParams,
    },
    task_run::{CreateTaskRunParams, TaskRunRepository},
    user::{User, UserRepository},
    verification_cache::{CachedVerification, VerificationCacheRepository},
    verification_result::{
        VerificationResultRepository, VerificationStepInsert, VerificationStepRow,
    },
};
