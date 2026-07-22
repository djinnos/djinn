use super::task_select_where_id;
use super::*;
use djinn_core::models::IssueType;

const REPEATED_REOPEN_MISMATCH_THRESHOLD: i64 = 3;

/// A deliberately narrow page row for the repeated-reopen mismatch scan.
///
/// Do not replace this with `Task`: mismatch inference needs only these task
/// columns, while `Task` hydration includes several correlated CI lookups.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BoardHealthMismatchCandidate {
    pub id: String,
    pub short_id: String,
    pub epic_id: Option<String>,
    pub title: String,
    pub description: String,
    pub design: String,
    pub acceptance_criteria: String,
    pub issue_type: String,
    pub status: String,
    pub total_reopen_count: i64,
}

mod board_health_scan;
pub use board_health_scan::{
    BOARD_HEALTH_MISMATCH_PAGE_SIZE, BoardHealthMismatchPage, BoardHealthMismatchScanState,
};

const PLANNER_ISSUE_TYPE_SIGNALS: &[&str] = &["planning", "decomposition"];

/// Text signals are authoritative for expected-role inference and are also
/// used to build the SQL prefilter. The prefilter may return false positives,
/// but every Rust-positive task must match it.
const PLANNER_ROLE_SIGNALS: &[(&str, &str)] = &[
    ("task_create", "requires:task_create"),
    ("epic_update", "requires:epic_update"),
    ("memory_ref", "requires:memory_ref_update"),
    ("memory refs", "requires:memory_ref_update"),
    ("re-priorit", "requires:reprioritization"),
    ("repriorit", "requires:reprioritization"),
    ("decompos", "requires:decomposition"),
    ("plan next wave", "requires:planning"),
    ("planning task", "requires:planning"),
];

const LEAD_ROLE_SIGNALS: &[(&str, &str)] = &[
    ("lead intervention", "requires:lead_intervention"),
    ("force_close", "requires:force_close"),
    ("replacement task", "requires:replacement_tasks"),
    ("decompose into subtasks", "requires:decomposition"),
];

trait RoleSignalTask {
    fn issue_type(&self) -> &str;
    fn title(&self) -> &str;
    fn description(&self) -> &str;
    fn design(&self) -> &str;
    fn acceptance_criteria(&self) -> &str;
}

macro_rules! impl_role_signal_task {
    ($type:ty) => {
        impl RoleSignalTask for $type {
            fn issue_type(&self) -> &str {
                &self.issue_type
            }
            fn title(&self) -> &str {
                &self.title
            }
            fn description(&self) -> &str {
                &self.description
            }
            fn design(&self) -> &str {
                &self.design
            }
            fn acceptance_criteria(&self) -> &str {
                &self.acceptance_criteria
            }
        }
    };
}

impl_role_signal_task!(Task);
impl_role_signal_task!(BoardHealthMismatchCandidate);

pub(super) fn toolset_for_role(role: &str) -> &'static [&'static str] {
    match role {
        "planner" => &[
            "task_create",
            "epic_update",
            "memory_ref_update",
            "reprioritization",
        ],
        "lead" => &[
            "lead_intervention",
            "force_close",
            "replacement_tasks",
            "decomposition",
        ],
        "reviewer" => &["task_review"],
        "architect" => &["board_health_review", "stuck_task_review"],
        _ => &["code_changes", "tests", "review_fix"],
    }
}

fn infer_expected_role_for_task(task: &impl RoleSignalTask) -> Option<(&'static str, Vec<String>)> {
    let mut planner_signals = Vec::new();

    if PLANNER_ISSUE_TYPE_SIGNALS
        .iter()
        .any(|issue_type| task.issue_type().eq_ignore_ascii_case(issue_type))
    {
        planner_signals.push("issue_type:planning".to_string());
        return Some(("planner", planner_signals));
    }

    let haystack = format!(
        "{}\n{}\n{}\n{}",
        task.title().to_lowercase(),
        task.description().to_lowercase(),
        task.design().to_lowercase(),
        task.acceptance_criteria().to_lowercase()
    );

    let add_signal_once = |signals: &mut Vec<String>, signal: &str| {
        if !signals.iter().any(|existing| existing == signal) {
            signals.push(signal.to_string());
        }
    };

    for (needle, signal) in PLANNER_ROLE_SIGNALS {
        if haystack.contains(needle) {
            add_signal_once(&mut planner_signals, signal);
        }
    }

    let mut lead_signals = Vec::new();
    for (needle, signal) in LEAD_ROLE_SIGNALS {
        if haystack.contains(needle) {
            add_signal_once(&mut lead_signals, signal);
        }
    }

    if !planner_signals.is_empty() {
        return Some(("planner", planner_signals));
    }
    if !lead_signals.is_empty() {
        return Some(("lead", lead_signals));
    }

    if IssueType::parse(task.issue_type())
        .map(|issue_type| issue_type.uses_simple_lifecycle())
        .unwrap_or(false)
    {
        return None;
    }

    None
}

fn dispatched_role_for_task(task: &BoardHealthMismatchCandidate) -> &'static str {
    match task.status.as_str() {
        "needs_task_review" | "in_task_review" => "reviewer",
        "needs_lead_intervention" | "in_lead_intervention" => "lead",
        _ => match task.issue_type.as_str() {
            "planning" | "decomposition" => "planner",
            "spike" | "review" => "architect",
            _ => "worker",
        },
    }
}

/// Pure authoritative evaluation for the persisted page runner.
pub fn evaluate_board_health_mismatch_candidate(task: &BoardHealthMismatchCandidate) -> bool {
    infer_expected_role_for_task(task)
        .is_some_and(|(expected_role, _)| expected_role != dispatched_role_for_task(task))
}

pub(super) fn board_health_mismatch_predicate(alias: &str) -> String {
    // This is a conservative prefilter, not role inference. It rejects only
    // impossible positives and deliberately preserves all Rust-positive rows.
    let text_expression = format!(
        "LOWER(CONCAT_WS(E'\\n', {alias}.title, {alias}.description, {alias}.design, {alias}.acceptance_criteria))"
    );
    let mut predicates = Vec::new();
    for issue_type in PLANNER_ISSUE_TYPE_SIGNALS {
        predicates.push(format!("LOWER({alias}.issue_type) = '{issue_type}'"));
    }
    for (needle, _) in PLANNER_ROLE_SIGNALS.iter().chain(LEAD_ROLE_SIGNALS) {
        predicates.push(format!("{text_expression} LIKE '%{needle}%'"));
    }
    format!(
        "{alias}.total_reopen_count >= {REPEATED_REOPEN_MISMATCH_THRESHOLD} \
         AND {alias}.status != 'closed' AND ({})",
        predicates.join(" OR ")
    )
}

#[cfg(test)]
async fn list_board_health_mismatch_candidates(
    repo: &TaskRepository,
) -> Result<Vec<BoardHealthMismatchCandidate>> {
    let sql = format!(
        "SELECT t.id, t.short_id, t.epic_id, t.title, t.description, t.design, \
                t.acceptance_criteria::text AS acceptance_criteria, t.issue_type, t.status, t.total_reopen_count \
         FROM tasks t WHERE {} ORDER BY t.id ASC",
        board_health_mismatch_predicate("t"),
    );
    Ok(sqlx::query_as::<_, BoardHealthMismatchCandidate>(&sql)
        .fetch_all(repo.db.pool())
        .await?)
}

impl TaskRepository {
    /// List tasks ready to start: status='open' with no unresolved blockers.
    pub async fn list_ready(&self, query: ReadyQuery) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        let (where_sql, params) = build_ready_where(&query, 0);
        let limit_placeholder = format!("${}", params.len() + 1);
        // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
        let sql = format!(
            "SELECT t.id, t.project_id, t.short_id, t.epic_id, t.title, t.description, t.design,
                    t.issue_type, t.status, t.priority, t.owner, t.labels::text AS labels,
                    t.acceptance_criteria::text AS acceptance_criteria, t.reopen_count, t.continuation_count,
                    t.total_reopen_count,
                    t.intervention_count, t.last_intervention_at,
                    t.created_at, t.updated_at, t.closed_at,
                    t.close_reason, t.merge_commit_sha, t.pr_url, t.merge_conflict_metadata, t.memory_refs::text AS memory_refs,
                    t.agent_type, t.created_by_user_id,
                    t.refinement_run_id, t.refinement_intent_id, t.refinement_generation,
                    t.refinement_round, t.refinement_phase, t.refinement_role,
                    COALESCE((SELECT s.ci_status FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1), 'unknown') AS ci_status,
                    (SELECT s.head_sha FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_head_sha,
                    (SELECT s.pr_number FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_pr_number,
                    COALESCE((SELECT s.blocking_required_check_names::text FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1), '[]') AS ci_blocking_required_check_names,
                    (SELECT s.failure_fingerprint FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_failure_fingerprint,
                    (SELECT s.first_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_first_seen_at,
                    (SELECT s.last_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_last_seen_at,
                    COALESCE((SELECT s.same_signature_count FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1), 0) AS ci_same_signature_count,
                    (SELECT s.last_remediation_base_sha FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_last_remediation_base_sha,
                    (SELECT s.mq_state FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_state,
                    (SELECT s.mq_run_id FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_run_id,
                    (SELECT s.mq_head_sha FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_head_sha,
                    (SELECT s.mq_failed_check_names::text FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_failed_check_names,
                    (SELECT s.mq_failure_fingerprint FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_failure_fingerprint,
                    (SELECT s.mq_same_signature_count FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_same_signature_count,
                    (SELECT s.mq_first_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_first_seen_at,
                    (SELECT s.mq_last_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_last_seen_at,
                    (SELECT ta.mirror_head_sha FROM task_attempts ta WHERE ta.task_id = t.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_mirror_head_sha,
                    (SELECT ta.github_head_sha FROM task_attempts ta WHERE ta.task_id = t.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_github_head_sha,
                    (SELECT CASE WHEN ta.mirror_head_sha IS NOT NULL AND ta.github_head_sha IS NOT NULL THEN ta.mirror_head_sha != ta.github_head_sha END FROM task_attempts ta WHERE ta.task_id = t.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_heads_diverged,
                    (SELECT ta.github_publication_error FROM task_attempts ta WHERE ta.task_id = t.id AND ta.github_publication_error IS NOT NULL ORDER BY ta.created_at DESC LIMIT 1) AS ci_head_observation_error
             FROM tasks t
             WHERE {where_sql}
             ORDER BY t.priority ASC, t.created_at ASC
             LIMIT {limit_placeholder}"
        );
        let mut q = sqlx::query_as::<_, Task>(&sql);
        for p in params {
            q = match p {
                SqlParam::Text(s) => q.bind(s),
                SqlParam::Integer(i) => q.bind(i),
            };
        }
        Ok(q.bind(query.limit).fetch_all(self.db.pool()).await?)
    }

    /// Atomically claim the highest-priority, oldest ready task and transition it
    /// to `in_progress`.
    ///
    /// "Ready" means `status = 'open'` with no unresolved blockers.  All filtering
    /// and the Start transition happen inside a single write transaction, so two
    /// concurrent callers can never claim the same task.
    ///
    /// Returns `None` when no task matches the query.
    pub async fn claim(
        &self,
        query: ReadyQuery,
        actor_id: &str,
        actor_role: &str,
    ) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;

        // Build the same WHERE as list_ready, then LIMIT 1 for the claim.
        let (where_sql, params) = build_ready_where(&query, 0);
        // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
        let sql = format!(
            "SELECT t.id, t.project_id, t.short_id, t.epic_id, t.title, t.description, t.design,
                    t.issue_type, t.status, t.priority, t.owner, t.labels::text AS labels,
                    t.acceptance_criteria::text AS acceptance_criteria, t.reopen_count, t.continuation_count,
                    t.total_reopen_count,
                    t.intervention_count, t.last_intervention_at,
                    t.created_at, t.updated_at, t.closed_at,
                    t.close_reason, t.merge_commit_sha, t.pr_url, t.merge_conflict_metadata, t.memory_refs::text AS memory_refs,
                    t.agent_type, t.created_by_user_id,
                    t.refinement_run_id, t.refinement_intent_id, t.refinement_generation,
                    t.refinement_round, t.refinement_phase, t.refinement_role,
                    COALESCE((SELECT s.ci_status FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1), 'unknown') AS ci_status,
                    (SELECT s.head_sha FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_head_sha,
                    (SELECT s.pr_number FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_pr_number,
                    COALESCE((SELECT s.blocking_required_check_names::text FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1), '[]') AS ci_blocking_required_check_names,
                    (SELECT s.failure_fingerprint FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_failure_fingerprint,
                    (SELECT s.first_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_first_seen_at,
                    (SELECT s.last_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_last_seen_at,
                    COALESCE((SELECT s.same_signature_count FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1), 0) AS ci_same_signature_count,
                    (SELECT s.last_remediation_base_sha FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_last_remediation_base_sha,
                    (SELECT s.mq_state FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_state,
                    (SELECT s.mq_run_id FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_run_id,
                    (SELECT s.mq_head_sha FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_head_sha,
                    (SELECT s.mq_failed_check_names::text FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_failed_check_names,
                    (SELECT s.mq_failure_fingerprint FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_failure_fingerprint,
                    (SELECT s.mq_same_signature_count FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_same_signature_count,
                    (SELECT s.mq_first_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_first_seen_at,
                    (SELECT s.mq_last_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = t.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_last_seen_at,
                    (SELECT ta.mirror_head_sha FROM task_attempts ta WHERE ta.task_id = t.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_mirror_head_sha,
                    (SELECT ta.github_head_sha FROM task_attempts ta WHERE ta.task_id = t.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_github_head_sha,
                    (SELECT CASE WHEN ta.mirror_head_sha IS NOT NULL AND ta.github_head_sha IS NOT NULL THEN ta.mirror_head_sha != ta.github_head_sha END FROM task_attempts ta WHERE ta.task_id = t.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_heads_diverged,
                    (SELECT ta.github_publication_error FROM task_attempts ta WHERE ta.task_id = t.id AND ta.github_publication_error IS NOT NULL ORDER BY ta.created_at DESC LIMIT 1) AS ci_head_observation_error
             FROM tasks t
             WHERE {where_sql}
             ORDER BY t.priority ASC, t.created_at ASC
             LIMIT 1"
        );
        let mut candidate_q = sqlx::query_as::<_, Task>(&sql);
        for p in params {
            candidate_q = match p {
                SqlParam::Text(s) => candidate_q.bind(s),
                SqlParam::Integer(i) => candidate_q.bind(i),
            };
        }

        let candidate: Option<Task> = candidate_q.fetch_optional(&mut *tx).await?;

        let task = match candidate {
            None => {
                tx.commit().await?;
                return Ok(None);
            }
            Some(t) => t,
        };

        // Apply Start transition: open → in_progress.
        sqlx::query!(
            r#"UPDATE tasks SET status = 'in_progress',
                 updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $1"#,
            task.id
        )
        .execute(&mut *tx)
        .await?;

        let activity_id = uuid::Uuid::now_v7().to_string();
        let payload = serde_json::json!({
            "from_status": "open",
            "to_status":   "in_progress",
        });
        sqlx::query!(
            "INSERT INTO activity_log
                (id, task_id, actor_id, actor_role, event_type, payload)
             VALUES ($1, $2, $3, $4, 'status_changed', $5)",
            activity_id,
            task.id,
            actor_id,
            actor_role,
            payload
        )
        .execute(&mut *tx)
        .await?;

        let task = task_select_where_id!(&task.id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        let task = Some(task);

        if let Some(ref t) = task {
            self.events.send(DjinnEventEnvelope::task_updated(t, false));
        }
        Ok(task)
    }

    /// List tasks with filters, sorting, and pagination.
    pub async fn list_filtered(&self, query: ListQuery) -> Result<ListResult> {
        self.db.ensure_initialized().await?;
        let (where_sql, params) = build_where(
            &query.project_id,
            &query.status,
            &query.issue_type,
            query.priority,
            &query.label,
            &query.text,
            &query.parent,
            0,
        );
        let order_sql = sort_to_sql(&query.sort);

        // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
        let total_sql = format!("SELECT COUNT(*) FROM tasks WHERE {where_sql}");
        let mut total_q = sqlx::query_scalar::<_, i64>(&total_sql);
        for p in &params {
            total_q = match p {
                SqlParam::Text(s) => total_q.bind(s.clone()),
                SqlParam::Integer(i) => total_q.bind(*i),
            };
        }
        let total = total_q.fetch_one(self.db.pool()).await?;

        let limit_ph = format!("${}", params.len() + 1);
        let offset_ph = format!("${}", params.len() + 2);
        // NOTE: dynamic SQL (WHERE + ORDER clauses built from request) — compile-time check not possible
        let sql = format!(
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels::text AS labels, acceptance_criteria::text AS acceptance_criteria,
                    reopen_count, continuation_count,
                    total_reopen_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs::text AS memory_refs,
                    agent_type, created_by_user_id, refinement_run_id, refinement_intent_id, refinement_generation, refinement_round, refinement_phase, refinement_role,
                    COALESCE((SELECT s.ci_status FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1), 'unknown') AS ci_status,
                    (SELECT s.head_sha FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_head_sha,
                    (SELECT s.pr_number FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_pr_number,
                    COALESCE((SELECT s.blocking_required_check_names::text FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1), '[]') AS ci_blocking_required_check_names,
                    (SELECT s.failure_fingerprint FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_failure_fingerprint,
                    (SELECT s.first_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_first_seen_at,
                    (SELECT s.last_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_last_seen_at,
                    COALESCE((SELECT s.same_signature_count FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1), 0) AS ci_same_signature_count,
                    (SELECT s.last_remediation_base_sha FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_last_remediation_base_sha,
                    (SELECT s.mq_state FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_state,
                    (SELECT s.mq_run_id FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_run_id,
                    (SELECT s.mq_head_sha FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_head_sha,
                    (SELECT s.mq_failed_check_names::text FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_failed_check_names,
                    (SELECT s.mq_failure_fingerprint FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_failure_fingerprint,
                    (SELECT s.mq_same_signature_count FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_same_signature_count,
                    (SELECT s.mq_first_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_first_seen_at,
                    (SELECT s.mq_last_seen_at FROM task_pr_ci_snapshots s WHERE s.task_id = tasks.id ORDER BY s.last_seen_at DESC LIMIT 1) AS ci_mq_last_seen_at,
                    (SELECT ta.mirror_head_sha FROM task_attempts ta WHERE ta.task_id = tasks.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_mirror_head_sha,
                    (SELECT ta.github_head_sha FROM task_attempts ta WHERE ta.task_id = tasks.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_github_head_sha,
                    (SELECT CASE WHEN ta.mirror_head_sha IS NOT NULL AND ta.github_head_sha IS NOT NULL THEN ta.mirror_head_sha != ta.github_head_sha END FROM task_attempts ta WHERE ta.task_id = tasks.id AND (ta.mirror_head_sha IS NOT NULL OR ta.github_head_sha IS NOT NULL OR ta.github_publication_error IS NOT NULL) ORDER BY ta.created_at DESC LIMIT 1) AS ci_heads_diverged,
                    (SELECT ta.github_publication_error FROM task_attempts ta WHERE ta.task_id = tasks.id AND ta.github_publication_error IS NOT NULL ORDER BY ta.created_at DESC LIMIT 1) AS ci_head_observation_error,
                    (SELECT COUNT(*) FROM blockers b
                     JOIN tasks bt ON b.blocking_task_id = bt.id
                     WHERE b.task_id = tasks.id AND bt.status != 'closed') AS unresolved_blocker_count
             FROM tasks WHERE {where_sql} ORDER BY {order_sql} LIMIT {limit_ph} OFFSET {offset_ph}"#
        );
        let mut task_q = sqlx::query_as::<_, Task>(&sql);
        for p in &params {
            task_q = match p {
                SqlParam::Text(s) => task_q.bind(s.clone()),
                SqlParam::Integer(i) => task_q.bind(*i),
            };
        }
        let tasks = task_q
            .bind(query.limit)
            .bind(query.offset)
            .fetch_all(self.db.pool())
            .await?;

        Ok(ListResult {
            tasks,
            total_count: total,
        })
    }

    /// Count tasks with optional grouping.
    pub async fn count_grouped(&self, query: CountQuery) -> Result<serde_json::Value> {
        self.db.ensure_initialized().await?;
        let (where_sql, params) = build_where(
            &query.project_id,
            &query.status,
            &query.issue_type,
            query.priority,
            &query.label,
            &query.text,
            &query.parent,
            0,
        );

        match query.group_by.as_deref() {
            Some(gb) => {
                let col = match gb {
                    "status" => "status",
                    "priority" => "priority",
                    "issue_type" => "issue_type",
                    "epic" => "epic_id",
                    other => {
                        return Err(Error::Internal(format!("unknown group_by: {other}")));
                    }
                };
                // NOTE: dynamic SQL (group_by column chosen at runtime) — compile-time check not possible
                let sql = format!(
                    "SELECT COALESCE(CAST({col} AS TEXT), ''), COUNT(*)
                     FROM tasks WHERE {where_sql}
                     GROUP BY {col} ORDER BY COUNT(*) DESC, {col}"
                );
                let mut groups_q = sqlx::query_as::<_, (String, i64)>(&sql);
                for p in &params {
                    groups_q = match p {
                        SqlParam::Text(s) => groups_q.bind(s.clone()),
                        SqlParam::Integer(i) => groups_q.bind(*i),
                    };
                }
                let groups = groups_q
                    .fetch_all(self.db.pool())
                    .await?
                    .into_iter()
                    .map(|(key, count)| serde_json::json!({"key": key, "count": count}))
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({ "groups": groups }))
            }
            None => {
                // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
                let sql = format!("SELECT COUNT(*) FROM tasks WHERE {where_sql}");
                let mut total_q = sqlx::query_scalar::<_, i64>(&sql);
                for p in &params {
                    total_q = match p {
                        SqlParam::Text(s) => total_q.bind(s.clone()),
                        SqlParam::Integer(i) => total_q.bind(*i),
                    };
                }
                let total = total_q.fetch_one(self.db.pool()).await?;
                Ok(serde_json::json!({ "total_count": total }))
            }
        }
    }

    /// Aggregate board health report: epic stats, stale in_progress tasks, review queue.
    pub async fn board_health(&self, stale_hours: i64) -> Result<serde_json::Value> {
        self.db.ensure_initialized().await?;
        // Per-epic task counts and % complete.
        let epic_rows = sqlx::query!(
            r#"SELECT e.id, e.short_id, e.title,
                    COUNT(t.id) AS total,
                    CAST(SUM(CASE WHEN t.status = 'closed' THEN 1 ELSE 0 END) AS BIGINT) AS closed,
                    CAST(SUM(CASE WHEN t.status IN (
                        'needs_task_review','in_task_review','closed'
                    ) THEN 1 ELSE 0 END) AS BIGINT) AS in_review,
                    MIN(CASE WHEN t.status IN (
                        'needs_task_review','in_task_review','closed'
                    ) THEN t.updated_at ELSE NULL END) AS oldest_review_at,
                    CAST(SUM(CASE WHEN t.status IN ('approved','pr_draft','pr_review') THEN 1 ELSE 0 END) AS BIGINT) AS pr_ready
             FROM epics e
             LEFT JOIN tasks t ON t.epic_id = e.id
             GROUP BY e.id
             ORDER BY e.title"#,
        )
        .fetch_all(self.db.pool())
        .await?;
        let epic_stats: Vec<serde_json::Value> = epic_rows
            .into_iter()
            .map(|row| {
                let total: i64 = row.total.unwrap_or(0);
                let closed: i64 = row.closed.unwrap_or(0);
                let in_review: i64 = row.in_review.unwrap_or(0);
                let oldest_review_at: Option<String> = row.oldest_review_at;
                let pr_ready: i64 = row.pr_ready.unwrap_or(0);
                let pct = if total > 0 {
                    (closed as f64 / total as f64 * 1000.0).round() / 10.0
                } else {
                    0.0
                };
                serde_json::json!({
                    "epic_id":          row.id,
                    "short_id":         row.short_id,
                    "title":            row.title,
                    "total":            total,
                    "closed":           closed,
                    "in_review":        in_review,
                    "pr_ready":         pr_ready,
                    "pct_complete":     pct,
                    "oldest_review_at": oldest_review_at,
                })
            })
            .collect();

        // Stale tasks: in_progress longer than the threshold.
        // NOTE: dynamic SQL (stale_hours inlined) — compile-time check not possible
        let stale_sql = format!(
            r#"SELECT t.id, t.short_id, t.title, t.status, t.updated_at, t.owner,
                    e.short_id AS epic_short_id
             FROM tasks t
             JOIN epics e ON t.epic_id = e.id
             WHERE t.status = 'in_progress'
               AND t.updated_at < to_char((now() at time zone 'utc') - (interval '1 hour' * {stale_hours}), 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             ORDER BY t.updated_at ASC"#
        );
        let stale_rows = sqlx::query(&stale_sql).fetch_all(self.db.pool()).await?;
        let stale_tasks: Vec<serde_json::Value> = stale_rows
            .into_iter()
            .map(|row| {
                use sqlx::Row;
                serde_json::json!({
                    "id":            row.get::<String, _>(0),
                    "short_id":      row.get::<String, _>(1),
                    "title":         row.get::<String, _>(2),
                    "status":        row.get::<String, _>(3),
                    "updated_at":    row.get::<String, _>(4),
                    "owner":         row.get::<String, _>(5),
                    "epic_short_id": row.get::<String, _>(6),
                })
            })
            .collect();

        // Review queue: tasks waiting in any review status. Closed tasks are
        // NOT part of a queue of pending review work, and including them made
        // this an unbounded full-history fetch (every closed task ever,
        // serialized into JSON on each board_health call — the UI polls this
        // every poll interval). Bounded defensively: the live review set is
        // WIP-sized, so the LIMIT only guards against pathological states.
        let review_rows = sqlx::query!(
            r#"SELECT t.id, t.short_id, t.title, t.status, t.updated_at,
                    e.short_id AS epic_short_id
             FROM tasks t
             JOIN epics e ON t.epic_id = e.id
             WHERE t.status IN ('needs_task_review','in_task_review','approved','pr_draft','pr_review')
             ORDER BY t.updated_at ASC
             LIMIT 500"#,
        )
        .fetch_all(self.db.pool())
        .await?;
        let review_queue: Vec<serde_json::Value> = review_rows
            .into_iter()
            .map(|row| {
                serde_json::json!({
                    "id":            row.id,
                    "short_id":      row.short_id,
                    "title":         row.title,
                    "status":        row.status,
                    "updated_at":    row.updated_at,
                    "epic_short_id": row.epic_short_id,
                })
            })
            .collect();

        // Refreshing this advisory finding is coordinator-owned and bounded to
        // persisted 250-row pages. Keep the legacy response field as an array,
        // but never materialize mismatch candidates on this request path.
        let repeated_reopen_role_tool_mismatches: Vec<serde_json::Value> = Vec::new();

        // ── Additive liveness / protocol / attribution / stranded sections ─
        // These bounded JSON sections live in the sibling `board_health`
        // module to keep this file under the repository size guard.
        let liveness_outcomes =
            super::board_health::liveness_outcomes_section(self.db.pool()).await;
        let protocol_violations =
            super::board_health::protocol_violations_section(self.db.pool()).await;
        let attribution_findings =
            super::board_health::attribution_findings_section(self.db.pool()).await;
        let stranded_ready = super::board_health::stranded_ready_section(self.db.pool()).await;
        let closed_parent_open_children =
            super::board_health::closed_parent_open_children_section(self.db.pool()).await;

        Ok(serde_json::json!({
            "epic_stats":            epic_stats,
            "stale_tasks":           stale_tasks,
            "review_queue":          review_queue,
            "repeated_reopen_role_tool_mismatches": repeated_reopen_role_tool_mismatches,
            "stale_threshold_hours": stale_hours,
            "liveness_outcomes":     liveness_outcomes,
            "protocol_violations":   protocol_violations,
            "attribution_findings":  attribution_findings,
            "stranded_ready":        stranded_ready,
            "closed_parent_open_children": closed_parent_open_children,
        }))
    }

    /// Heal stale tasks: move `in_progress` tasks older than `stale_hours` back to `open`.
    /// Logs a `status_changed` activity entry for each healed task and emits `TaskUpdated` events.
    pub async fn reconcile(&self, stale_hours: i64) -> Result<serde_json::Value> {
        let events_tx = self.events.clone();
        self.db.ensure_initialized().await?;

        // Dolt surfaces error 1213 (serialization failure) when this
        // transaction overlaps with any other writer to `tasks` /
        // `activity_log`. The failure is benign — re-running the whole
        // reconcile loop on a fresh transaction succeeds once the
        // contending writer has committed. See `djinn_db::retry` for
        // the retry contract.
        let healed: Vec<Task> = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || async {
                let mut tx = self.db.pool().begin().await?;
                // NOTE: dynamic SQL (stale_hours inlined) — compile-time check not possible
                let sql = format!(
                    r#"SELECT id
                     FROM tasks
                     WHERE status = 'in_progress'
                       AND updated_at < to_char((now() at time zone 'utc') - (interval '1 hour' * {stale_hours}), 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#
                );
                // Reconciliation only needs IDs to perform the update. Hydrate
                // each changed row through the canonical projection below so a
                // Task cannot be decoded from a stale partial projection.
                let stale_ids: Vec<String> = sqlx::query_scalar(&sql).fetch_all(&mut *tx).await?;

                let mut healed: Vec<Task> = Vec::new();
                for task_id in stale_ids {
                    sqlx::query!(
                        r#"UPDATE tasks
                         SET status = 'open',
                             updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $1"#,
                        task_id
                    )
                    .execute(&mut *tx)
                    .await?;

                    let activity_id = uuid::Uuid::now_v7().to_string();
                    let payload = serde_json::json!({
                        "from":   "in_progress",
                        "to":     "open",
                        "reason": "reconcile_stale",
                    });
                    sqlx::query!(
                        "INSERT INTO activity_log
                            (id, task_id, actor_id, actor_role, event_type, payload)
                         VALUES ($1, $2, 'system', 'system', 'status_changed', $3)",
                        activity_id,
                        task_id,
                        payload
                    )
                    .execute(&mut *tx)
                    .await?;

                    let updated: Task = task_select_where_id!(&task_id).fetch_one(&mut *tx).await?;
                    healed.push(updated);
                }
                tx.commit().await?;
                Ok::<_, crate::Error>(healed)
            },
        )
        .await?;

        for task in &healed {
            events_tx.send(DjinnEventEnvelope::task_updated(task, false));
        }

        let healed_short_ids: Vec<&str> = healed.iter().map(|t| t.short_id.as_str()).collect();
        Ok(serde_json::json!({
            "healed_tasks":      healed.len(),
            "healed_task_ids":   healed_short_ids,
            "recovered_tasks":   0,
            "reviews_triggered": 0,
        }))
    }
}

// ── Dynamic query helpers ─────────────────────────────────────────────────────

/// Build a SQL WHERE clause + params vector from optional filter fields.
///
/// Returns `("1=1", [])` when no filters are supplied. Placeholders are
/// numbered Postgres-style starting at `${param_offset + 1}` so the caller can
/// safely append additional binds (e.g. LIMIT/OFFSET) after this clause.
// Each arg is an independent, optional SQL filter; folding them into a struct
// would just shift the same fields around for no clarity gain.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_where(
    project_id: &Option<String>,
    status: &Option<String>,
    issue_type: &Option<String>,
    priority: Option<i64>,
    label: &Option<String>,
    text: &Option<String>,
    parent: &Option<String>,
    param_offset: usize,
) -> (String, Vec<SqlParam>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlParam> = Vec::new();

    if let Some(p) = project_id {
        let ph = format!("${}", param_offset + params.len() + 1);
        clauses.push(format!("project_id = {ph}"));
        params.push(SqlParam::Text(p.clone()));
    }

    if let Some(s) = status {
        if s == "merged" {
            // Pseudo-status backing the Kanban "Merged" column: closed AND
            // actually merged. This MUST stay in sync with the UI's
            // `taskToColumnKey` (ui/src/components/KanbanBoard.tsx), which does:
            //
            //     const merged =
            //       task.merge_commit_sha != null ||
            //       (task.pr_url != null && task.close_reason === "completed");
            //     return merged ? "done" : null;   // (only when status closed)
            //
            // All literals are fixed constants (not user input), so no binds.
            clauses.push(
                "status = 'closed' AND (merge_commit_sha IS NOT NULL \
                 OR (pr_url IS NOT NULL AND close_reason = 'completed'))"
                    .to_owned(),
            );
        } else if let Some(neg) = s.strip_prefix('!') {
            let ph = format!("${}", param_offset + params.len() + 1);
            clauses.push(format!("status != {ph}"));
            params.push(SqlParam::Text(neg.to_owned()));
        } else {
            let ph = format!("${}", param_offset + params.len() + 1);
            clauses.push(format!("status = {ph}"));
            params.push(SqlParam::Text(s.clone()));
        }
    }

    if let Some(it) = issue_type {
        if let Some(neg) = it.strip_prefix('!') {
            let ph = format!("${}", param_offset + params.len() + 1);
            clauses.push(format!("issue_type != {ph}"));
            params.push(SqlParam::Text(neg.to_owned()));
        } else {
            let ph = format!("${}", param_offset + params.len() + 1);
            clauses.push(format!("issue_type = {ph}"));
            params.push(SqlParam::Text(it.clone()));
        }
    }

    if let Some(p) = priority {
        let ph = format!("${}", param_offset + params.len() + 1);
        clauses.push(format!("priority = {ph}"));
        params.push(SqlParam::Integer(p));
    }

    if let Some(lbl) = label {
        let ph = format!("${}", param_offset + params.len() + 1);
        // Postgres JSONB array membership; `labels` is JSONB on Postgres.
        clauses.push(format!("labels @> jsonb_build_array({ph}::text)"));
        params.push(SqlParam::Text(lbl.clone()));
    }

    if let Some(t) = text {
        let ph_a = format!("${}", param_offset + params.len() + 1);
        let ph_b = format!("${}", param_offset + params.len() + 2);
        // ILIKE (not LIKE): case-insensitive substring match. This filter backs
        // the Kanban board's search box (which fetches unloaded merged tasks via
        // task_list?text=…), where a case-sensitive match is surprising — a
        // search for "auth" should find "Auth" and "AUTHZ" alike.
        clauses.push(format!("(title ILIKE {ph_a} OR description ILIKE {ph_b})"));
        let pattern = format!("%{t}%");
        params.push(SqlParam::Text(pattern.clone()));
        params.push(SqlParam::Text(pattern));
    }

    if let Some(p) = parent {
        let ph = format!("${}", param_offset + params.len() + 1);
        clauses.push(format!("epic_id = {ph}"));
        params.push(SqlParam::Text(p.clone()));
    }

    let where_sql = if clauses.is_empty() {
        "1=1".to_owned()
    } else {
        clauses.join(" AND ")
    };

    (where_sql, params)
}

/// Build the WHERE clause for `list_ready` / `claim`: the fixed
/// `t.status = 'open' AND no-unresolved-blockers` predicate plus optional
/// filters. Placeholders are numbered Postgres-style starting at
/// `${param_offset + 1}`.
pub(super) fn build_ready_where(
    query: &ReadyQuery,
    param_offset: usize,
) -> (String, Vec<SqlParam>) {
    let mut clauses: Vec<String> = vec![
        "t.status = 'open'".to_owned(),
        "NOT EXISTS (
             SELECT 1 FROM blockers b2
             JOIN tasks bt ON bt.id = b2.blocking_task_id
             WHERE b2.task_id = t.id
                AND bt.status != 'closed'
         )"
        .to_owned(),
        // Hold every task belonging to a frozen proposal build out of dispatch.
        // `t.epic_id` is NULL for the epic_breakdown task and any epic-less task,
        // so `pe.epic_id = t.epic_id` yields no match and they stay dispatchable.
        "NOT EXISTS (
             SELECT 1 FROM proposal_epics pe
             JOIN proposals p ON p.id = pe.proposal_id
             WHERE pe.epic_id = t.epic_id
                AND p.build_frozen = true
         )"
        .to_owned(),
        // Refinement (tribunal) tasks are driven by the dedicated refinement
        // dispatch loop, never the worker ready pass. Excluding them here keeps
        // them out of worker dispatch entirely (they are owner-less by design,
        // so they would otherwise be repeatedly refused by the ownership guard).
        "t.issue_type != 'refinement'".to_owned(),
    ];
    let mut params: Vec<SqlParam> = Vec::new();

    if let Some(project_id) = &query.project_id {
        let ph = format!("${}", param_offset + params.len() + 1);
        clauses.push(format!("t.project_id = {ph}"));
        params.push(SqlParam::Text(project_id.clone()));
    }
    if let Some(it) = &query.issue_type {
        if let Some(neg) = it.strip_prefix('!') {
            let ph = format!("${}", param_offset + params.len() + 1);
            clauses.push(format!("t.issue_type != {ph}"));
            params.push(SqlParam::Text(neg.to_owned()));
        } else {
            let ph = format!("${}", param_offset + params.len() + 1);
            clauses.push(format!("t.issue_type = {ph}"));
            params.push(SqlParam::Text(it.clone()));
        }
    }
    if let Some(lbl) = &query.label {
        let ph = format!("${}", param_offset + params.len() + 1);
        clauses.push(format!("t.labels @> jsonb_build_array({ph}::text)"));
        params.push(SqlParam::Text(lbl.clone()));
    }
    if let Some(owner) = &query.owner {
        let ph = format!("${}", param_offset + params.len() + 1);
        clauses.push(format!("t.owner = {ph}"));
        params.push(SqlParam::Text(owner.clone()));
    }
    if let Some(pmax) = query.priority_max {
        let ph = format!("${}", param_offset + params.len() + 1);
        clauses.push(format!("t.priority <= {ph}"));
        params.push(SqlParam::Integer(pmax));
    }

    (clauses.join(" AND "), params)
}

/// Convert a sort key to a SQL ORDER BY clause.
pub(super) fn sort_to_sql(sort: &str) -> &'static str {
    match sort {
        "created" => "created_at ASC",
        "created_desc" => "created_at DESC",
        "updated" => "updated_at ASC",
        "updated_desc" => "updated_at DESC",
        "closed" => "closed_at DESC, created_at DESC",
        _ => "priority ASC, created_at ASC", // default: "priority"
    }
}

#[cfg(test)]
mod board_health_bounds_tests;
#[cfg(test)]
mod board_health_scan_protocol_tests;
#[cfg(test)]
mod merged_classification_tests;
#[cfg(test)]
mod ready_projection_tests;
