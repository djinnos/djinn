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
    DatabaseConnectConfig, NoteSearchBackend, NoteVectorBackend, PostgresDatabaseConfig,
    SqliteVecStatus, default_db_path,
};
pub use error::{DbError as Error, DbResult as Result};
pub use repositories::{
    agent::{
        AgentCreateInput, AgentListQuery, AgentListResult, AgentMetrics, AgentRepository,
        AgentUpdateInput, LearnedPromptHistoryEntry, PendingAmendmentEvaluation, VALID_BASE_ROLES,
        WindowedRoleMetrics,
    },
    code_chunk::{
        ChunkAndEmbedReport, CodeChunk, CodeChunkEmbeddingProvider, CodeChunkRepairEmbeddingRow,
        CodeChunkRepository, CodeChunkSearchHit, CodeChunkVectorBackend, CodeChunkVectorMatch,
        CodeChunkVectorStore, EmbeddedCodeChunk, InflightGuard, NoopCodeChunkVectorStore,
        QdrantCodeChunkConfig, QdrantCodeChunkVectorStore, UpsertCodeChunkEmbedding, cap_per_file,
        chunk_and_embed_files, hydrate_chunk_ids, lexical_search_chunks,
        qdrant_code_chunk_point_id_hex, try_claim_project,
    },
    commit_file_changes::{
        CommitFileChange, CommitFileChangeRepository, CoupledFile, CoupledPair, CouplingHub,
        CouplingPairEvent, FileChurn, MAX_FILES_PER_COMMIT_FOR_PAIRS, coupling_event_key,
        derive_pair_events, derive_pair_events_into,
    },
    dispatch_pause::{DispatchPauseMutation, DispatchPauseRepository, DispatchPauseTarget},
    dispatch_state::{DispatchStateRecord, DispatchStateRepository, DispatchStateUpsert},
    doctor_finding::{
        DoctorFinding, DoctorFindingRepository, MAX_RECENT_FINDINGS, NewDoctorFinding,
        RecentDoctorFindings, severity as doctor_severity,
    },
    epic::{
        EpicBlockerRef, EpicCountQuery, EpicCreateInput, EpicListQuery, EpicListResult,
        EpicRepository, EpicTaskCounts, EpicUpdateInput,
    },
    events::EventsRepository,
    git_settings::GitSettingsRepository,
    image::{Image, ImageRepository, ImageStatus},
    models::ModelsRepository,
    note::{
        AnchorProposerKind, BackfillRetrievalAnchorOptions, BackfillRetrievalAnchorReport,
        CONTRADICTION, ConsolidatedNoteProvenance, ConsolidationCandidateEdge,
        ConsolidationCluster, ConsolidationNote, ConsolidationRunMetric, ContradictionCandidate,
        CreateCanonicalConsolidatedNote, CreateConsolidationRunMetric,
        CreatedCanonicalConsolidatedNote, DbNoteGroup, EmbeddedNote, LexicalSearchBackend,
        LexicalSearchMode, LexicalSearchPlan, LlmAnchorProposer, NoopNoteVectorStore,
        NoteAssociationEntry, NoteConsolidationRepository, NoteDedupCandidate, NoteEmbeddingMatch,
        NoteEmbeddingProvider, NoteEmbeddingRecord, NoteQualityAssessment, NoteRepairEmbeddingRow,
        NoteRepository, NoteSearchParams, NoteVectorStore, PromptBudgetReport,
        ProposedBackfillAnchor, QdrantConfig, QdrantNoteVectorStore, QueryReplayReport,
        RankingReport, ReplayCriteria, ReplayFixture, ReplayNote, ReplayQuery, ReplayReport,
        STALE_CITATION, STALE_DECAY_SIGNAL, UpsertNoteEmbedding, anchor_embedding_replay_fixture,
        assess_note_quality, build_lexical_search_plan, decay_signal_for_elapsed_days,
        embedding_content_hash, embedding_document_text, executable_lexical_search_sql,
        folder_for_type, folder_for_type_with_status, generate_anchor_embedding_replay_report,
        infer_embedding_branch_from_worktree, infer_note_type, is_singleton,
        legacy_embedding_document_text, lexical_search_threshold, looks_task_local,
        normalize_lexical_score, normalize_virtual_note_path, permalink_for,
        permalink_for_with_status, permalink_from_virtual_note_path, propose_anchor_deterministic,
        render_anchor_embedding_replay_report_markdown, render_note_markdown, required_sections,
        rrf_fuse, sanitize_postgres_tsquery, sanitize_sqlite_fts5_query, slugify, task_branch_name,
        title_from_permalink, validate_postgres_tsvector_threshold,
        virtual_note_path_for_permalink,
    },
    oauth::{
        AuthorizationCode, McpAccessToken, NewAccessToken, NewAuthorizationCode, NewOAuthClient,
        OAuthClient, OAuthRepository,
    },
    org_config::{NewOrgConfig, OrgConfig, OrgConfigRepository},
    project::{
        DispatchImage, ProjectConfig, ProjectDispatchReadiness, ProjectImage, ProjectImageStatus,
        ProjectRepository,
    },
    project_workspace_graph::{
        CODELESS_WORKSPACE_SLUG, ProjectWorkspaceGraph, ProjectWorkspaceGraphLatest,
        ProjectWorkspaceGraphRepository, ProjectWorkspaceGraphUpsert,
    },
    proposal::{
        ProposalCreateInput, ProposalFeedbackCreateInput, ProposalListQuery, ProposalListResult,
        ProposalRepository, ProposalUpdateInput,
    },
    repo_graph_cache::{CachedRepoGraph, RepoGraphCacheInsert, RepoGraphCacheRepository},
    service::{ServicePreset, ServicePresetRepository},
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
    usage_analytics::{
        BreakdownRow, DailySeriesRow, GroupDimension, ModelEffectivenessRow, ProjectModelMatrixRow,
        UsageAnalyticsQuery, UsageAnalyticsRepository, UsageAnalyticsResult, UsageTotals,
    },
    user::{User, UserRepository},
    user_settings::UserSettingsRepository,
    verification::VerificationRepository,
    verification_cache::{CachedVerification, VerificationCacheRepository},
    verification_result::{
        VerificationResultRepository, VerificationStepInsert, VerificationStepRow,
    },
    verification_run::{VerificationRun, VerificationRunRepository, VerificationRunStatus},
    verification_test::{VerificationTestRepository, VerificationTestRun, VerificationTestStatus},
};
