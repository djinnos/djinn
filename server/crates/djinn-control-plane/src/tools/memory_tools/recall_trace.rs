use super::*;
use djinn_db::NoteRepository;
use djinn_db::repositories::retrieval_trace::{
    CandidateOutcome, RetrievalTraceEntryPoint, RetrievalTraceListFilter, RetrievalTraceRepository,
    SkippedReason,
};
use rmcp::{Json, handler::server::wrapper::Parameters, tool, tool_router};
const EXCERPT: usize = 1000;
/// Maximum number of retained traces a single list request may retrieve.
pub(crate) const MAX_RECALL_TRACE_LIST_LIMIT: i32 = 100;
#[tool_router(router = memory_recall_trace_router, vis = "pub(super)")]
impl DjinnMcpServer {
    #[tool(
        description = "Browse persisted memory retrieval traces. Use mode=list with project or project_id and filters (limit maximum: 100), or mode=detail with trace_id. Lists are compact; detail returns one trace with bounded note excerpts."
    )]
    pub async fn memory_recall_trace(
        &self,
        Parameters(p): Parameters<RecallTraceParams>,
    ) -> Json<MemoryRecallTraceResponse> {
        Json(recall(self, p).await)
    }
}
fn err(e: impl Into<String>) -> MemoryRecallTraceResponse {
    MemoryRecallTraceResponse {
        traces: vec![],
        trace: None,
        error: Some(e.into()),
    }
}
fn entry(v: Option<&str>) -> Result<Option<RetrievalTraceEntryPoint>, String> {
    v.map(|x| RetrievalTraceEntryPoint::parse(x).ok_or_else(|| format!("invalid entry_point: {x}")))
        .transpose()
}
fn outcome(v: Option<&str>) -> Result<Option<CandidateOutcome>, String> {
    v.map(|x| match x {
        "injected" => Ok(CandidateOutcome::Injected),
        "skipped" => Ok(CandidateOutcome::Skipped),
        _ => Err(format!("invalid outcome: {x}")),
    })
    .transpose()
}
fn reason(v: Option<&str>) -> Result<Option<SkippedReason>, String> {
    v.map(|x| SkippedReason::parse(x).ok_or_else(|| format!("invalid skipped_reason: {x}")))
        .transpose()
}
fn excerpt(v: &str) -> String {
    let mut x: String = v.chars().take(EXCERPT).collect();
    if v.chars().count() > EXCERPT {
        x.push('…');
    }
    x
}
pub(super) async fn recall(s: &DjinnMcpServer, p: RecallTraceParams) -> MemoryRecallTraceResponse {
    let Some(project) = p.project_id.as_deref().or(p.project.as_deref()) else {
        return err("project or project_id parameter required");
    };
    let project_id = match super::ops::resolve_project_id(s, project).await {
        Ok(x) => x,
        Err(e) => return err(e),
    };
    let repo = RetrievalTraceRepository::new(s.state.db().clone());
    if p.mode == "list" {
        if p.limit
            .is_some_and(|limit| limit > MAX_RECALL_TRACE_LIST_LIMIT)
        {
            return err(format!(
                "limit must be at most {MAX_RECALL_TRACE_LIST_LIMIT}"
            ));
        }
        let ep = match entry(p.entry_point.as_deref()) {
            Ok(x) => x,
            Err(e) => return err(e),
        };
        let out = match outcome(p.outcome.as_deref()) {
            Ok(x) => x,
            Err(e) => return err(e),
        };
        let skip = match reason(p.skipped_reason.as_deref()) {
            Ok(x) => x,
            Err(e) => return err(e),
        };
        let rows = match repo
            .list_by_project(
                &project_id,
                RetrievalTraceListFilter {
                    session_id: p.session_id.as_deref(),
                    task_id: p.task_id.as_deref(),
                    task_run_id: p.task_run_id.as_deref(),
                    entry_point: ep,
                    outcome: out,
                    skipped_reason: skip,
                    limit: p.limit,
                    offset: p.offset,
                },
            )
            .await
        {
            Ok(x) => x,
            Err(e) => return err(e.to_string()),
        };
        let traces = rows
            .into_iter()
            .map(|r| {
                let c = r.candidates_typed();
                let injected = c
                    .iter()
                    .filter(|x| x.outcome == CandidateOutcome::Injected)
                    .count() as i32;
                MemoryRecallTraceSummary {
                    trace_id: r.id,
                    created_at: r.created_at,
                    entry_point: r.entry_point,
                    trigger_summary: r
                        .trigger
                        .map(|x| x.to_string().chars().take(240).collect())
                        .unwrap_or_default(),
                    candidate_count: c.len() as i32,
                    injected_count: injected,
                    skipped_count: c.len() as i32 - injected,
                    candidate_cap_exceeded: r.candidate_cap_exceeded,
                }
            })
            .collect();
        return MemoryRecallTraceResponse {
            traces,
            trace: None,
            error: None,
        };
    }
    if p.mode != "detail" {
        return err("mode must be 'list' or 'detail'");
    }
    let Some(id) = p.trace_id.as_deref() else {
        return err("trace_id parameter required for detail mode");
    };
    let r = match repo.get_by_id(id).await {
        Ok(Some(x)) if x.project_id == project_id => x,
        Ok(_) => return err(format!("retrieval trace not found: {id}")),
        Err(e) => return err(e.to_string()),
    };
    let notes = NoteRepository::new(s.state.db().clone(), s.state.event_bus());
    let mut candidates = Vec::new();
    for c in r.candidates_typed() {
        let n = notes
            .get(&c.note_id)
            .await
            .ok()
            .flatten()
            .filter(|n| n.project_id == project_id);
        let title = n
            .as_ref()
            .map(|n| n.title.clone())
            .or(c.title)
            .unwrap_or_default();
        let permalink = n
            .as_ref()
            .map(|n| n.permalink.clone())
            .or(c.permalink)
            .unwrap_or_default();
        candidates.push(MemoryRecallTraceCandidate {
            note_id: c.note_id,
            title,
            permalink,
            content_excerpt: n.as_ref().map(|n| excerpt(&n.content)),
            rank: c.rank,
            confidence: c.confidence,
            outcome: c.outcome.as_str().to_string(),
            skipped_reason: c.skipped_reason.map(|x| x.as_str().to_string()),
            source: c.source,
            scope: c.scope,
        });
    }
    MemoryRecallTraceResponse {
        traces: vec![],
        trace: Some(MemoryRecallTraceDetail {
            trace_id: r.id,
            schema_version: r.schema_version,
            created_at: r.created_at,
            session_id: r.session_id,
            task_id: r.task_id,
            task_run_id: r.task_run_id,
            entry_point: r.entry_point,
            trigger: r.trigger,
            durations_ms: r.durations_ms,
            candidate_cap: r.candidate_cap,
            candidate_cap_exceeded: r.candidate_cap_exceeded,
            sampling_metadata: r.sampling_metadata,
            estimated_injected_tokens: r.estimated_injected_tokens,
            candidates,
        }),
        error: None,
    }
}
