use djinn_core::clock::{Clock, SystemClock};
use djinn_db::NoteSearchParams;
use djinn_db::repositories::retrieval_trace::{
    RetrievalTraceRepository, TaxonomyV1RetrievalHealthGroup,
};
use djinn_db::{
    NoteRepository, ProjectRepository, normalize_virtual_note_path,
    permalink_from_virtual_note_path, resolve_short_ids,
};
use djinn_telemetry::memory_retrieval::{
    MemoryRetrievalMetrics, RetrievalEntryPoint, RetrievalOutcome, RetrievalStage,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::server::DjinnMcpServer;

use super::{
    BrokenLinksParams, BuildContextParams, ExtractedAuditParams, HealthParams, ListParams,
    MemoryBrokenLinksResponse, MemoryBuildContextResponse, MemoryExtractedAuditResponse,
    MemoryHealthResponse, MemoryListResponse, MemoryNoteResponse, MemoryOrphansResponse,
    MemoryProposalOverview, MemoryRecallTraceResponse, MemoryRetrievalOutcomesReportResponse,
    MemorySearchResponse, MemorySearchResultItem, OrphansParams, ReadParams, RecallTraceParams,
    ResolvedMention, RetrievalEntryPointHealthSummary, RetrievalHealthResponse,
    RetrievalHealthScope, RetrievalOutcomesReportParams, RetrievalTaxonomyValidationErrorSummary,
    SearchParams, TaxonomyV1DispositionSummary, TaxonomyV1RetrievalHealthSummary, note_to_view,
    parse_proposal_ref_item, parse_task_ref_item,
};

pub async fn memory_recall_trace(
    server: &DjinnMcpServer,
    p: RecallTraceParams,
) -> MemoryRecallTraceResponse {
    super::recall_trace::recall(server, p).await
}
pub async fn memory_retrieval_outcomes_report(
    server: &DjinnMcpServer,
    p: RetrievalOutcomesReportParams,
) -> MemoryRetrievalOutcomesReportResponse {
    super::retrieval_outcomes_report::report(server, p).await
}

fn normalize_folder_filter(folder: Option<String>) -> Option<String> {
    folder.and_then(|value| {
        let trimmed = value.trim();
        let without_scheme = trimmed.strip_prefix("memory://").unwrap_or(trimmed);
        let normalized = normalize_virtual_note_path(without_scheme);
        (!normalized.is_empty()).then_some(normalized)
    })
}

/// Observer guard that records exactly one retrieval outcome and any optional
/// stage durations for the bounded telemetry dimensions.
pub(crate) struct RetrievalObserver {
    entry_point: RetrievalEntryPoint,
    started_at: Instant,
    metrics: Arc<MemoryRetrievalMetrics>,
    finished: bool,
}

impl RetrievalObserver {
    pub(crate) fn new(server: &DjinnMcpServer, entry_point: RetrievalEntryPoint) -> Self {
        Self {
            entry_point,
            started_at: SystemClock::new().now_instant(),
            metrics: server.state.retrieval_metrics(),
            finished: false,
        }
    }

    pub(crate) fn observe_embedding(&self, duration: Duration) {
        self.observe_stage(RetrievalStage::Embedding, Some(duration));
    }

    pub(crate) fn observe_stage(&self, stage: RetrievalStage, duration: Option<Duration>) {
        if let Some(duration) = duration {
            let _ = self
                .metrics
                .observe_stage(self.entry_point, stage, duration);
        }
    }

    pub(crate) fn finish(mut self, outcome: RetrievalOutcome, candidates: u64) {
        let _ = self.metrics.observe(
            self.entry_point,
            outcome,
            self.started_at.elapsed(),
            candidates,
        );
        self.finished = true;
    }
}

impl Drop for RetrievalObserver {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.metrics.observe(
                self.entry_point,
                RetrievalOutcome::Error,
                self.started_at.elapsed(),
                0,
            );
        }
    }
}

fn permalink_candidates(identifier: &str) -> Vec<String> {
    let trimmed = identifier.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let without_scheme = trimmed.strip_prefix("memory://").unwrap_or(trimmed);
    let normalized = normalize_virtual_note_path(without_scheme);
    if normalized.is_empty() {
        return Vec::new();
    }

    let mut candidates = vec![normalized.clone()];
    if let Some(permalink) = permalink_from_virtual_note_path(&normalized)
        && permalink != normalized
    {
        candidates.push(permalink);
    }
    candidates
}

async fn record_retrieved_notes(
    server: &DjinnMcpServer,
    repo: &NoteRepository,
    note_ids: &[String],
) {
    // ADR-054 retrieval boundary: notes returned to the MCP client count as accessed,
    // even if the client does not issue a follow-up `memory_read`. This keeps search
    // result retrieval flows visible to temporal/co-access scoring without touching
    // notes that were only considered internally during ranking.
    for note_id in note_ids {
        let _ = repo.touch_accessed(note_id).await;
        server.record_memory_read(note_id).await;
    }
}

pub async fn memory_extracted_audit(
    server: &DjinnMcpServer,
    p: ExtractedAuditParams,
) -> MemoryExtractedAuditResponse {
    let project_id = match resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => {
            return MemoryExtractedAuditResponse {
                scanned_note_count: None,
                merge_candidates: None,
                underspecified: None,
                demote_to_working_spec: None,
                archive_candidates: None,
                rerun_hint: None,
                error: Some(error),
            };
        }
    };

    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_vector_store(server.state.vector_store());
    match repo.extracted_note_audit(&project_id).await {
        Ok(report) => MemoryExtractedAuditResponse {
            scanned_note_count: Some(report.scanned_note_count),
            merge_candidates: Some(report.merge_candidates),
            underspecified: Some(report.underspecified),
            demote_to_working_spec: Some(report.demote_to_working_spec),
            archive_candidates: Some(report.archive_candidates),
            rerun_hint: Some(report.rerun_hint),
            error: None,
        },
        Err(error) => MemoryExtractedAuditResponse {
            scanned_note_count: None,
            merge_candidates: None,
            underspecified: None,
            demote_to_working_spec: None,
            archive_candidates: None,
            rerun_hint: None,
            error: Some(error.to_string()),
        },
    }
}

pub async fn resolve_project_id(server: &DjinnMcpServer, project: &str) -> Result<String, String> {
    let repo = ProjectRepository::new(server.state.db().clone(), server.state.event_bus());
    repo.resolve(project)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("project not found: {project}"))
}

/// Scan `content` for 4-char alphanumeric short_id patterns and resolve them
/// against proposals, epics, and tasks. Filters out common false positives
/// (inline code, URLs, hex-like strings).
fn extract_short_id_candidates(content: &str) -> Vec<String> {
    let re = regex::Regex::new(r#"[a-z0-9]{4}"#).expect("short_id regex should compile");
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for mat in re.find_iter(content) {
        let candidate = mat.as_str();

        // Skip if inside backtick code (inline or fenced)
        // Simple heuristic: count backticks before this position on the same line
        let line_start = content[..mat.start()]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let line = &content[line_start..mat.start()];
        let backticks = line.matches('`').count();
        if backticks % 2 == 1 {
            continue;
        }

        // Skip if part of a URL (preceded by http:// or https:// or similar)
        let before = &content[..mat.start()];
        if before
            .rfind("http://")
            .map(|i| i + 7 > line_start)
            .unwrap_or(false)
            || before
                .rfind("https://")
                .map(|i| i + 8 > line_start)
                .unwrap_or(false)
        {
            continue;
        }

        // Skip pure hex-like strings (all digits or a-f only, common in commit hashes/UUIDs)
        let is_hex = candidate
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
        if is_hex {
            continue;
        }

        if seen.insert(candidate.to_string()) {
            candidates.push(candidate.to_string());
        }
    }

    candidates
}

pub async fn memory_read(server: &DjinnMcpServer, p: ReadParams) -> MemoryNoteResponse {
    let project_id = match resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => return MemoryNoteResponse::error(error),
    };

    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_vector_store(server.state.vector_store());
    let note = if let Some(note) = {
        let mut exact = None;
        for candidate in permalink_candidates(&p.identifier) {
            if let Ok(Some(note)) = repo.get_by_permalink(&project_id, &candidate).await {
                exact = Some(note);
                break;
            }
        }
        exact
    } {
        note
    } else {
        match repo
            .search(NoteSearchParams {
                project_id: &project_id,
                query: &p.identifier,
                task_id: None,
                folder: None,
                note_type: None,
                limit: 1,
                semantic_scores: None,
                edge_kinds: None,
                entity_types: None,
            })
            .await
        {
            Ok(results) if !results.is_empty() => {
                match repo.get(&results[0].id).await.ok().flatten() {
                    Some(note) => note,
                    None => {
                        return MemoryNoteResponse::error(format!(
                            "note not found: {}",
                            p.identifier
                        ));
                    }
                }
            }
            _ => return MemoryNoteResponse::error(format!("note not found: {}", p.identifier)),
        }
    };

    let _ = repo.touch_accessed(&note.id).await;
    if note.abstract_.is_none() || note.overview.is_none() {
        server.enqueue_missing_summary_backfill(&note.id).await;
    }
    server.record_memory_read(&note.id).await;

    let mut response = MemoryNoteResponse::from_note(&note);

    // Surface task refs and reachable proposals for this note
    if let Some(ref permalink) = response.permalink {
        response.tasks = repo
            .task_refs(permalink)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(parse_task_ref_item)
            .collect();
        response.proposals = repo
            .proposal_refs(permalink)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(parse_proposal_ref_item)
            .collect();
    }

    // Resolve short_id mentions in note body
    if let Some(ref content) = response.content {
        let candidates = extract_short_id_candidates(content);
        if !candidates.is_empty()
            && let Ok(entities) = resolve_short_ids(server.state.db(), &candidates).await
        {
            response.resolved_mentions = entities
                .into_iter()
                .map(|e| ResolvedMention {
                    short_id: e.short_id,
                    entity_type: e.entity_type,
                    title: e.title,
                    permalink: e.permalink,
                })
                .collect();
        }
    }

    response
}

pub async fn memory_search(
    server: &DjinnMcpServer,
    p: SearchParams,
    task_id: Option<&str>,
) -> MemorySearchResponse {
    let observer = RetrievalObserver::new(server, RetrievalEntryPoint::Dispatch);

    let project_id = match resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => {
            observer.finish(RetrievalOutcome::Error, 0);
            return MemorySearchResponse {
                results: vec![],
                error: Some(error),
            };
        }
    };

    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_vector_store(server.state.vector_store());
    let limit = p.limit.unwrap_or(10).clamp(1, 100) as usize;

    let embed_start = SystemClock::new().now_instant();
    let embedding = server.state.embed_memory_query(&p.query).await;
    let embed_duration = embed_start.elapsed();
    observer.observe_embedding(embed_duration);
    let semantic_scores = match embedding {
        Ok(Some(embedding)) => repo
            .semantic_candidate_scores(
                &project_id,
                &embedding.values,
                task_id,
                p.folder.as_deref(),
                p.note_type.as_deref(),
                limit,
            )
            .await
            .ok(),
        Ok(None) | Err(_) => None,
    };

    match repo
        .search_with_stats(NoteSearchParams {
            project_id: &project_id,
            query: &p.query,
            task_id,
            folder: p.folder.as_deref(),
            note_type: p.note_type.as_deref(),
            limit,
            semantic_scores,
            edge_kinds: p.edge_kinds.as_deref(),
            entity_types: p.entity_types.as_deref(),
        })
        .await
    {
        Ok(timed) => {
            observer.observe_stage(RetrievalStage::Lexical, timed.lexical_duration);
            observer.observe_stage(RetrievalStage::Semantic, timed.semantic_duration);
            observer.observe_stage(RetrievalStage::Temporal, timed.temporal_duration);
            observer.observe_stage(RetrievalStage::Graph, timed.graph_duration);
            observer.observe_stage(RetrievalStage::RrfFuse, timed.rrf_fuse_duration);

            let results = timed.rows;
            let outcome = if results.is_empty() {
                RetrievalOutcome::Empty
            } else {
                RetrievalOutcome::Success
            };
            let candidates = timed.summary.candidate_count as u64;

            let retrieved_note_ids: Vec<String> = results
                .iter()
                .filter(|r| r.entity == "note")
                .map(|result| result.id.clone())
                .collect();
            record_retrieved_notes(server, &repo, &retrieved_note_ids).await;

            let response = MemorySearchResponse {
                results: results
                    .into_iter()
                    .map(|r| MemorySearchResultItem {
                        id: r.id,
                        permalink: r.permalink,
                        title: r.title,
                        folder: r.folder,
                        note_type: r.note_type,
                        snippet: r.snippet,
                        entity: r.entity,
                        score: r.score,
                    })
                    .collect(),
                error: None,
            };
            observer.finish(outcome, candidates);
            response
        }
        Err(e) => {
            let response = MemorySearchResponse {
                results: vec![],
                error: Some(format!("search failed: {e}")),
            };
            observer.finish(RetrievalOutcome::Error, 0);
            response
        }
    }
}

pub async fn memory_list(server: &DjinnMcpServer, p: ListParams) -> MemoryListResponse {
    let project_id = match resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => {
            return MemoryListResponse {
                notes: vec![],
                error: Some(error),
            };
        }
    };

    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_vector_store(server.state.vector_store());
    let depth = p.depth.unwrap_or(1);
    let folder = normalize_folder_filter(p.folder);
    let notes = repo
        .list_compact(
            &project_id,
            folder.as_deref(),
            p.note_type.as_deref(),
            depth,
            p.status.as_deref(),
        )
        .await
        .unwrap_or_default();

    MemoryListResponse { notes, error: None }
}

pub async fn memory_build_context(
    server: &DjinnMcpServer,
    p: BuildContextParams,
    task_id: Option<&str>,
) -> MemoryBuildContextResponse {
    let observer = RetrievalObserver::new(server, RetrievalEntryPoint::LoadKnowledgeContext);

    let project_id = match resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => {
            observer.finish(RetrievalOutcome::Error, 0);
            return MemoryBuildContextResponse {
                primary: vec![],
                related_l1: vec![],
                related_l0: vec![],
                supersedes: vec![],
                contradicts: vec![],
                proposals: vec![],
                error: Some(error),
            };
        }
    };

    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_vector_store(server.state.vector_store());
    let max_related = p.max_related.unwrap_or(10).clamp(1, 50) as usize;
    let budget = p.budget.map(|b| b as usize);
    let url = p.url.strip_prefix("memory://").unwrap_or(&p.url);

    if url.ends_with("/*") {
        let folder = url.trim_end_matches("/*");
        let folder_filter = if folder.is_empty() {
            None
        } else {
            Some(folder)
        };
        match repo.list(&project_id, folder_filter).await {
            Ok(all) => {
                let response = MemoryBuildContextResponse {
                    primary: all.into_iter().map(|n| note_to_view(&n)).collect(),
                    related_l1: vec![],
                    related_l0: vec![],
                    supersedes: vec![],
                    contradicts: vec![],
                    proposals: vec![],
                    error: None,
                };
                let outcome = if response.primary.is_empty() {
                    RetrievalOutcome::Empty
                } else {
                    RetrievalOutcome::Success
                };
                observer.finish(outcome, response.primary.len() as u64);
                return response;
            }
            Err(error) => {
                let response = MemoryBuildContextResponse {
                    primary: vec![],
                    related_l1: vec![],
                    related_l0: vec![],
                    supersedes: vec![],
                    contradicts: vec![],
                    proposals: vec![],
                    error: Some(format!("build_context failed: {error}")),
                };
                observer.finish(RetrievalOutcome::Error, 0);
                return response;
            }
        }
    }

    match repo
        .build_context(
            &project_id,
            url,
            budget,
            task_id,
            max_related,
            p.min_confidence,
            p.edge_kinds.as_deref(),
        )
        .await
    {
        Ok(response) => {
            let response = MemoryBuildContextResponse {
                primary: response.primary.iter().map(note_to_view).collect(),
                related_l1: response.related_l1,
                related_l0: response.related_l0,
                supersedes: response.supersedes,
                contradicts: response.contradicts,
                proposals: response
                    .proposals
                    .into_iter()
                    .map(|p| MemoryProposalOverview {
                        id: p.id,
                        short_id: p.short_id,
                        title: p.title,
                        body_format: p.body_format,
                        acceptance_criteria: p.acceptance_criteria,
                        status: p.status,
                        score: p.score,
                    })
                    .collect(),
                error: None,
            };
            let candidates = response.primary.len()
                + response.related_l1.len()
                + response.related_l0.len()
                + response.proposals.len();
            let outcome = if response.primary.is_empty() {
                RetrievalOutcome::Empty
            } else {
                RetrievalOutcome::Success
            };
            observer.finish(outcome, candidates as u64);
            response
        }
        Err(e) => {
            let response = MemoryBuildContextResponse {
                primary: vec![],
                related_l1: vec![],
                related_l0: vec![],
                supersedes: vec![],
                contradicts: vec![],
                proposals: vec![],
                error: Some(format!("build_context failed: {e}")),
            };
            observer.finish(RetrievalOutcome::Error, 0);
            response
        }
    }
}

fn health_timestamp(value: OffsetDateTime) -> String {
    value
        .format(&Rfc3339)
        .expect("UTC timestamps format as RFC3339")
}

fn unavailable_retrieval_scope(
    error_code: &str,
    window_start: String,
    window_end: String,
    started_at: Option<String>,
) -> RetrievalHealthScope {
    RetrievalHealthScope {
        status: "unavailable".to_string(),
        error_code: Some(error_code.to_string()),
        window_start: Some(window_start),
        window_end: Some(window_end),
        started_at,
        groups: vec![],
        process_summaries: vec![],
    }
}

fn process_retrieval_scope(server: &DjinnMcpServer, until: OffsetDateTime) -> RetrievalHealthScope {
    let metrics = server.state.retrieval_metrics();
    let started_at = health_timestamp(OffsetDateTime::from(metrics.started_at()));
    let until = health_timestamp(until);
    match metrics.snapshot() {
        Ok(snapshot) => RetrievalHealthScope {
            status: "available".to_string(),
            error_code: None,
            window_start: Some(started_at.clone()),
            window_end: Some(until),
            started_at: Some(started_at),
            groups: vec![],
            process_summaries: RetrievalEntryPoint::ALL
                .iter()
                .map(|entry_point| {
                    let success = snapshot.aggregate(*entry_point, RetrievalOutcome::Success);
                    let empty = snapshot.aggregate(*entry_point, RetrievalOutcome::Empty);
                    let error = snapshot.aggregate(*entry_point, RetrievalOutcome::Error);
                    RetrievalEntryPointHealthSummary {
                        entry_point: match entry_point {
                            RetrievalEntryPoint::Dispatch => "dispatch",
                            RetrievalEntryPoint::JitPitfalls => "jit_pitfalls",
                            RetrievalEntryPoint::LoadKnowledgeContext => "load_knowledge_context",
                            RetrievalEntryPoint::FormatKnowledgeNotes => "format_knowledge_notes",
                        }
                        .to_string(),
                        total_queries: (success.count + empty.count + error.count) as i64,
                        error_queries: error.count as i64,
                        candidate_count: (success.candidate_sum
                            + empty.candidate_sum
                            + error.candidate_sum) as i64,
                        injected_count: 0,
                        skipped_count: 0,
                    }
                })
                .collect(),
        },
        Err(_) => unavailable_retrieval_scope(
            "process_snapshot_unavailable",
            started_at.clone(),
            until,
            Some(started_at),
        ),
    }
}

pub(super) fn taxonomy_v1_summary(
    group: TaxonomyV1RetrievalHealthGroup,
) -> TaxonomyV1RetrievalHealthSummary {
    let counts = group.counts;
    let zero_candidate_queries = counts.zero_candidate_queries;
    TaxonomyV1RetrievalHealthSummary {
        project_id: group.project_id,
        entry_point: group.entry_point.as_str().to_string(),
        taxonomy_version: group.taxonomy_version,
        window_start: group.window_start,
        window_end: group.window_end,
        refreshed_at: group.refreshed_at,
        invalid: group.invalid,
        total_queries: counts.total_queries,
        successful_queries: counts.successful_queries,
        errored_queries: counts.errored_queries,
        zero_candidate_queries,
        // One-release wire compatibility only: it is deliberately identical to
        // zero_candidate_queries, never the historical zero-injected metric.
        zero_result_queries: zero_candidate_queries,
        candidate_bearing_queries: counts.candidate_bearing_queries,
        starved_queries: counts.starved_queries,
        injected_queries: counts.injected_queries,
        candidate_total: counts.candidate_total,
        injected_total: counts.injected_total,
        dispositions: TaxonomyV1DispositionSummary {
            confidence_filtered_total: counts.confidence_filtered_total,
            not_top_k_total: counts.not_top_k_total,
            oversized_skipped_total: counts.oversized_skipped_total,
            injected_total: counts.injected_disposition_total,
            budget_pruned_total: counts.budget_pruned_total,
        },
        legacy_unclassified_queries: counts.legacy_unclassified_queries,
        invalid_taxonomy_queries: counts.invalid_taxonomy_queries,
        validation_errors: group
            .validation_errors
            .into_iter()
            .map(|error| RetrievalTaxonomyValidationErrorSummary {
                trace_id: error.trace_id,
                reason: error.reason,
            })
            .collect(),
    }
}

fn empty_memory_health(retrieval: RetrievalHealthResponse, error: String) -> MemoryHealthResponse {
    MemoryHealthResponse {
        total_notes: None,
        broken_link_count: None,
        orphan_note_count: None,
        authored_orphan_count: None,
        isolated_count: None,
        isolated_pct: None,
        machine_connected_orphan_count: None,
        low_confidence_note_count: None,
        stale_note_count: None,
        stale_notes_by_folder: None,
        lifecycle: None,
        recent_sweep: None,
        retrieval,
        error: Some(error),
    }
}

pub async fn memory_health(server: &DjinnMcpServer, p: HealthParams) -> MemoryHealthResponse {
    let config = server.state.retrieval_config();
    let until = OffsetDateTime::now_utc();
    let from = until - time::Duration::hours(config.window_hours() as i64);
    let from_text = health_timestamp(from);
    let until_text = health_timestamp(until);
    let process = process_retrieval_scope(server, until);
    let project = match p.project {
        Some(project) => project,
        None => {
            let persisted =
                unavailable_retrieval_scope("project_required", from_text, until_text, None);
            return empty_memory_health(
                RetrievalHealthResponse {
                    config_window_hours: config.window_hours() as i64,
                    persisted,
                    process,
                },
                "project parameter required".to_string(),
            );
        }
    };
    let project_id = match resolve_project_id(server, &project).await {
        Ok(id) => id,
        Err(error) => {
            let persisted =
                unavailable_retrieval_scope("project_unresolved", from_text, until_text, None);
            return empty_memory_health(
                RetrievalHealthResponse {
                    config_window_hours: config.window_hours() as i64,
                    persisted,
                    process,
                },
                error,
            );
        }
    };
    let trace_repo = RetrievalTraceRepository::new(server.state.db().clone());
    let persisted = match trace_repo
        .taxonomy_v1_health_rollup(&from_text, &until_text, &until_text)
        .await
    {
        Ok(groups) => RetrievalHealthScope {
            status: "available".to_string(),
            error_code: None,
            window_start: Some(from_text),
            window_end: Some(until_text),
            started_at: None,
            // The repository query is already independently grouped by project
            // and entry point. Select the requested project; never pool it.
            groups: groups
                .into_iter()
                .filter(|group| group.project_id == project_id)
                .map(taxonomy_v1_summary)
                .collect(),
            process_summaries: vec![],
        },
        Err(_) => unavailable_retrieval_scope("rollup_unavailable", from_text, until_text, None),
    };
    let retrieval = RetrievalHealthResponse {
        config_window_hours: config.window_hours() as i64,
        persisted,
        process,
    };
    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_vector_store(server.state.vector_store());
    match repo.health(&project_id).await {
        Ok(h) => MemoryHealthResponse {
            total_notes: Some(h.total_notes),
            broken_link_count: Some(h.broken_link_count),
            orphan_note_count: Some(h.orphan_note_count),
            authored_orphan_count: Some(h.authored_orphan_count),
            isolated_count: Some(h.isolated_count),
            isolated_pct: Some(h.isolated_pct),
            machine_connected_orphan_count: Some(h.machine_connected_orphan_count),
            low_confidence_note_count: Some(h.low_confidence_note_count),
            stale_note_count: Some(h.stale_note_count),
            stale_notes_by_folder: Some(h.stale_notes_by_folder),
            lifecycle: Some(h.lifecycle),
            recent_sweep: Some(h.recent_sweep),
            retrieval,
            error: None,
        },
        Err(error) => empty_memory_health(retrieval, error.to_string()),
    }
}

pub async fn memory_broken_links(
    server: &DjinnMcpServer,
    p: BrokenLinksParams,
) -> MemoryBrokenLinksResponse {
    let project_id = match resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => {
            return MemoryBrokenLinksResponse {
                broken_links: vec![],
                error: Some(error),
            };
        }
    };
    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_vector_store(server.state.vector_store());
    let folder = normalize_folder_filter(p.folder);
    let broken_links = repo
        .broken_links(&project_id, folder.as_deref())
        .await
        .unwrap_or_default();
    MemoryBrokenLinksResponse {
        broken_links,
        error: None,
    }
}

pub async fn memory_orphans(server: &DjinnMcpServer, p: OrphansParams) -> MemoryOrphansResponse {
    let project_id = match resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => {
            return MemoryOrphansResponse {
                orphans: vec![],
                error: Some(error),
            };
        }
    };
    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_vector_store(server.state.vector_store());
    let folder = normalize_folder_filter(p.folder);
    let orphans = repo
        .orphans(&project_id, folder.as_deref())
        .await
        .unwrap_or_default();
    MemoryOrphansResponse {
        orphans,
        error: None,
    }
}
