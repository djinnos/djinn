pub mod background;
pub mod crypto;
pub mod database;
pub mod error;
pub mod migrations;
pub mod note_hash;
pub mod repositories;
pub mod retry;
pub mod short_id;
mod template_bootstrap;

pub mod test_support {
    pub use crate::repositories::test_support::{
        HousekeepingFixture, HousekeepingFixtureExpectedCounts, HousekeepingFixtureProject,
        UsageTestSessionSeed, UsageTestTaskSeed, add_blocker_edge,
        backdate_task_attempt_created_at, backdate_task_updated_at,
        build_multi_project_housekeeping_fixture, close_task_at,
        corrupt_credential_encrypted_value, drop_table_cascade_for_test, drop_table_for_test,
        ensure_doctor_findings_schema, event_bus_for, make_project,
        nullify_note_confidence_for_test, override_debate_trail_body_metadata,
        reject_new_task_arbitrations_for_test, rename_note_confidence_column_for_test,
        seed_chat_session_row, seed_project, seed_session_row, seed_session_row_with_id,
        seed_task_row,
    };
}

pub use database::{
    Database, DatabaseBackendCapabilities, DatabaseBackendKind, DatabaseBootstrapInfo,
    DatabaseConnectConfig, NoteSearchBackend, NoteVectorBackend, PostgresDatabaseConfig,
    SqliteVecStatus, default_db_path,
};
pub use error::{DbError as Error, DbResult as Result};
pub use repositories::tool_call_evaluator::{
    Decision, EvalInput, GateResult, GateThresholds, GoStopReport, ManualAuditResult, SampleMinima,
    WindowSpec, evaluate, matched_baseline_rows,
};
pub use repositories::tool_call_export::{
    ExportDimensions, NormalizedToolCallRow, PersistedTranscript, ToolCallExportRepository,
    normalize_persisted_transcript,
};
pub use repositories::tool_call_metrics::{
    AdoptionCounts, AdoptionShare, ConfidenceInterval, FailureRates, RateMetric,
    ToolSurfaceMetrics, adoption_counts, apply_patch_adoption_share, compute_metrics,
    edit_minus_apply_patch_failure_interval, failure_rates, read_truncation_loop_rate,
    retry_after_edit_failure, wilson_difference_interval, wilson_interval,
};
pub use repositories::{
    agent::{
        AgentCreateInput, AgentListQuery, AgentListResult, AgentMetrics, AgentRepository,
        AgentUpdateInput, VALID_BASE_ROLES,
    },
    audit_sampler::{
        AuditOutcomeKind, AuditOutcomeReportRow, AuditOutcomeRow, AuditSamplerRepository,
        AuditStratum, CreateSampleFrameParams, CreateSamplePolicyParams, CreateSelectionParams,
        MergedChangeRow, RecordOutcomeParams, SampleFrameRow, SamplePolicyRow, SelectionRow,
        UnmaterializedSelection, UpsertMergedChangeParams,
    },
    chat_interruption_notice::{
        ChatInterruptionNotice, ChatInterruptionNoticeRepository, CreateChatInterruptionNotice,
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
    extension_load_diagnostic::{ExtensionLoadDiagnosticRepository, InsertExtensionLoadDiagnostic},
    git_settings::GitSettingsRepository,
    image::{Image, ImageRepository, ImageStatus, SelectedCatalogImage},
    liveness::{
        ClaimExtensionRecord, CurrentLivenessState, LivenessEvidenceSnapshot, LivenessRepository,
    },
    llm_call_attempt::{
        CreateLlmCallAttemptParams, FinalizeLlmCallAttemptParams, LlmCallAttemptRecord,
        LlmCallAttemptRepository, LlmCallOutcome,
    },
    models::ModelsRepository,
    note::{
        AnchorProposerKind, BackfillRetrievalAnchorOptions, BackfillRetrievalAnchorReport,
        CONTRADICTION, ConsolidatedNoteProvenance, ConsolidationCandidateEdge,
        ConsolidationCluster, ConsolidationNote, ConsolidationRunMetric, ContradictionCandidate,
        CreateCanonicalConsolidatedNote, CreateConsolidationRunMetric,
        CreatedCanonicalConsolidatedNote, DbNoteGroup, EligibleEmbeddingNote, EmbeddedNote,
        EmbeddingAssociationRefreshStats, EmbeddingCandidate, LexicalSearchBackend,
        LexicalSearchMode, LexicalSearchPlan, LlmAnchorProposer, MemoryEntityAssociation,
        MemoryEntityKind, MemoryEntityRef, MemoryEntityType, NoopNoteVectorStore,
        NoteAssociationEntry, NoteAssociationKind, NoteAssociationProvenanceRow,
        NoteAssociationProvenanceUpsert, NoteAssociationSource, NoteConsolidationRepository,
        NoteDedupCandidate, NoteEmbeddingMatch, NoteEmbeddingProvider, NoteEmbeddingRecord,
        NoteQualityAssessment, NoteRepairEmbeddingRow, NoteRepository, NoteRevisionActorKind,
        NoteRevisionCreateState, NoteRevisionDesiredState, NoteRevisionEventInput,
        NoteRevisionEventKind, NoteRevisionEventRow, NoteRevisionMutation,
        NoteRevisionMutationResult, NoteRevisionReason, NoteRevisionSnapshot,
        NoteRevisionSubsystem, NoteRevisionValidationError, NoteSearchParams, NoteStatus,
        NoteVectorStore, PromptBudgetReport, ProposedBackfillAnchor, QdrantConfig,
        QdrantNoteVectorStore, QueryReplayReport, RankingReport, ReplayCriteria, ReplayFixture,
        ReplayNote, ReplayQuery, ReplayReport, STALE_CITATION, STALE_DECAY_SIGNAL,
        TrustedNoteRevisionAttribution, TrustedNoteRevisionProvenance, UpsertNoteEmbedding,
        anchor_embedding_replay_fixture, assess_note_quality, build_lexical_search_plan,
        decay_signal_for_elapsed_days, embedding_content_hash, embedding_document_text,
        executable_lexical_search_sql, folder_for_type, folder_for_type_with_status,
        generate_anchor_embedding_replay_report, infer_embedding_branch_from_worktree,
        infer_note_type, is_singleton, legacy_embedding_document_text, lexical_search_threshold,
        looks_task_local, normalize_lexical_score, normalize_virtual_note_path, permalink_for,
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
    org_ai_policy::OrgAiPolicyRepository,
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
        AwaitingReviewPark, NeedsEvidenceCapStatus, NeedsEvidenceClaimLink, ProposalCreateInput,
        ProposalDebateTrailCreateInput, ProposalFeedbackCreateInput, ProposalListQuery,
        ProposalListResult, ProposalListSummaryRow, ProposalMemoryRef, ProposalRef,
        ProposalRepository, ProposalUpdateInput,
    },
    repo_graph_cache::{CachedRepoGraph, RepoGraphCacheInsert, RepoGraphCacheRepository},
    scip_indexer_timing::{
        ScipIndexerTiming, ScipIndexerTimingObservation, ScipIndexerTimingRepository,
        TIMING_STATUS_FAILED, TIMING_STATUS_SUCCESS, TIMING_STATUS_TIMED_OUT,
    },
    service::{ServicePreset, ServicePresetRepository},
    session::{
        CreateSessionParams, ExtractionBackfillCandidate, OrphanSessionCandidate,
        SessionRepository, SessionStatusSnapshot,
    },
    session_auth::{CreateUserAuthSession, SessionAuthRepository, UserAuthSessionRecord},
    session_compaction_boundary::{
        BeginCompactionParams, CompactionBoundary, CompactionPhase, CompleteCompactionParams,
        SessionCompactionBoundaryRepository,
    },
    session_message::SessionMessageRepository,
    settings::SettingsRepository,
    task::{
        ActivityQuery, BlockerRef, ChildDisposition, CountQuery, CreateTaskInProjectParams,
        CreateTaskParams, DispositionCounts, DispositionFinding, DispositionPlan, DispositionScope,
        ListQuery, ListResult, ReadyQuery, UpdateTaskParams,
    },
    task::{EffectiveCreatorProvenance, TaskRepository},
    task_arbitration::{
        ArbitrationState, CreateArbitrationParams, TaskArbitrationRecord,
        TaskArbitrationRepository, TryCreateResult, UpdateDispatchLedgerParams,
    },
    task_attempt::{
        CompletedParentSummary, CreateTaskAttemptParams, FillTaskAttemptParams,
        GuardAdoptedPrTaskAttemptParams, GuardDeferTaskAttemptParams, OrphanedPendingAttempt,
        ReworkMarkerTaskAttemptParams, SubmitTaskAttemptParams, TaskAttemptRepository,
        TerminalTaskAttemptParams,
    },
    task_run::{CreateTaskRunParams, TaskRunRepository},
    usage_analytics::{
        EntityBreakdownRow, GroupDimension, ModelEffectivenessRow, ProjectModelMatrixRow,
        SeriesDetailRow, UsageAnalyticsQuery, UsageAnalyticsRepository, UsageTotals,
    },
    user::{User, UserRepository},
    user_settings::UserSettingsRepository,
    warm_base_activity::{WarmBaseActivity, WarmBaseActivityRepository},
};
pub use short_id::{ResolvedEntity, resolve_short_ids};
