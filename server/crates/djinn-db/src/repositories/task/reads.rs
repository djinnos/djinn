// djinn:allow-oversize
use super::task_select_where_id;
use super::*;

use tracing::warn;

impl TaskRepository {
    /// List all tasks in a project (for peer reconciliation - SYNC-14).
    pub async fn list_by_project(&self, project_id: &str) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels::text AS labels, acceptance_criteria::text AS acceptance_criteria,
                    reopen_count, continuation_count,
                    total_reopen_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs::text AS memory_refs,
                    agent_type, created_by_user_id,
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
                    CAST(0 AS BIGINT) AS unresolved_blocker_count
             FROM tasks WHERE project_id = $1 ORDER BY priority, created_at"#,
        ).bind(project_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_by_epic(&self, epic_id: &str) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels::text AS labels, acceptance_criteria::text AS acceptance_criteria,
                    reopen_count, continuation_count,
                    total_reopen_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs::text AS memory_refs,
                    agent_type, created_by_user_id,
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
                    CAST(0 AS BIGINT) AS unresolved_blocker_count
             FROM tasks WHERE epic_id = $1 ORDER BY priority, created_at"#,
        ).bind(epic_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_by_status(&self, status: &str) -> Result<Vec<Task>> {
        self.list_by_status_filtered(status, false).await
    }

    /// Like `list_by_status`, but when `exclude_blocked` is true, narrows to the
    /// dispatch-readiness view: omits tasks that have unresolved blockers AND
    /// tasks belonging to a frozen proposal build. (UI listings pass `false`, so
    /// frozen tasks stay visible there — only dispatch holds them.)
    pub async fn list_by_status_filtered(
        &self,
        status: &str,
        exclude_blocked: bool,
    ) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        let blocker_filter = if exclude_blocked {
            "AND NOT EXISTS (SELECT 1 FROM blockers b JOIN tasks bt ON b.blocking_task_id = bt.id WHERE b.task_id = tasks.id AND bt.status != 'closed')
             AND NOT EXISTS (SELECT 1 FROM proposal_epics pe JOIN proposals p ON p.id = pe.proposal_id WHERE pe.epic_id = tasks.epic_id AND p.build_frozen = true)"
        } else {
            ""
        };
        // NOTE: dynamic SQL (optional blocker_filter fragment) — compile-time check not possible
        let sql = format!(
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels::text AS labels, acceptance_criteria::text AS acceptance_criteria,
                    reopen_count, continuation_count,
                    total_reopen_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs::text AS memory_refs,
                    agent_type, created_by_user_id,
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
                    (SELECT ta.github_publication_error FROM task_attempts ta WHERE ta.task_id = tasks.id AND ta.github_publication_error IS NOT NULL ORDER BY ta.created_at DESC LIMIT 1) AS ci_head_observation_error
             FROM tasks WHERE status = $1 {blocker_filter} ORDER BY priority, created_at"#
        );
        Ok(sqlx::query_as::<_, Task>(&sql)
            .bind(status)
            .fetch_all(self.db.pool())
            .await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(task_select_where_id!(id)
            .fetch_optional(self.db.pool())
            .await?)
    }

    pub async fn get_by_short_id(&self, short_id: &str) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels::text AS labels, acceptance_criteria::text AS acceptance_criteria,
                    reopen_count, continuation_count,
                    total_reopen_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs::text AS memory_refs,
                    agent_type, created_by_user_id,
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
                    CAST(0 AS BIGINT) AS unresolved_blocker_count
             FROM tasks WHERE short_id = $1"#,
        ).bind(short_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve a task by UUID or short_id.
    pub async fn resolve(&self, id_or_short: &str) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels::text AS labels, acceptance_criteria::text AS acceptance_criteria,
                    reopen_count, continuation_count,
                    total_reopen_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs::text AS memory_refs,
                    agent_type, created_by_user_id,
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
                    CAST(0 AS BIGINT) AS unresolved_blocker_count
             FROM tasks WHERE id = $1 OR short_id = $2"#,
        ).bind(id_or_short).bind(id_or_short)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn resolve_in_project(
        &self,
        project_id: &str,
        id_or_short: &str,
    ) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels::text AS labels, acceptance_criteria::text AS acceptance_criteria,
                    reopen_count, continuation_count,
                    total_reopen_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs::text AS memory_refs,
                    agent_type, created_by_user_id,
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
                    CAST(0 AS BIGINT) AS unresolved_blocker_count
             FROM tasks WHERE project_id = $1 AND (id = $2 OR short_id = $3)"#,
        ).bind(project_id).bind(id_or_short).bind(id_or_short)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Find tasks whose `memory_refs` JSONB array contains the given permalink.
    ///
    /// Uses the JSONB containment operator (`@>`) so the lookup can be
    /// driven by a GIN index if/when we add one.
    pub async fn list_by_memory_ref(&self, permalink: &str) -> Result<Vec<Task>> {
        let probe = serde_json::Value::Array(vec![serde_json::Value::String(permalink.to_owned())]);
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Task>(
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels::text AS labels, acceptance_criteria::text AS acceptance_criteria,
                    reopen_count, continuation_count,
                    total_reopen_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs::text AS memory_refs,
                    agent_type, created_by_user_id,
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
                    CAST(0 AS BIGINT) AS unresolved_blocker_count
             FROM tasks WHERE memory_refs @> $1
             ORDER BY priority, created_at"#,
        ).bind(sqlx::types::Json(probe))
        .fetch_all(self.db.pool())
        .await?)
    }

    /// List tasks eligible for sync export (SYNC-12).
    ///
    /// Includes all non-closed tasks plus tasks closed within the last hour.
    /// Tasks closed longer than 1 hour ago are evicted from the export to keep
    /// JSONL files small.
    pub async fn list_for_export(&self, project_id: Option<&str>) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        // NOTE: dynamic SQL (SELECT variant depends on project filter) — compile-time check not possible
        let sql = if project_id.is_some() {
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels::text AS labels, acceptance_criteria::text AS acceptance_criteria,
                    reopen_count, continuation_count,
                    total_reopen_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs::text AS memory_refs,
                    agent_type, created_by_user_id,
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
                    (SELECT ta.github_publication_error FROM task_attempts ta WHERE ta.task_id = tasks.id AND ta.github_publication_error IS NOT NULL ORDER BY ta.created_at DESC LIMIT 1) AS ci_head_observation_error
             FROM tasks
             WHERE project_id = $1
               AND (status != 'closed' OR closed_at > to_char((now() at time zone 'utc') - interval '1 hour', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
             ORDER BY priority, created_at"#
        } else {
            r#"SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels::text AS labels, acceptance_criteria::text AS acceptance_criteria,
                    reopen_count, continuation_count,
                    total_reopen_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs::text AS memory_refs,
                    agent_type, created_by_user_id,
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
                    (SELECT ta.github_publication_error FROM task_attempts ta WHERE ta.task_id = tasks.id AND ta.github_publication_error IS NOT NULL ORDER BY ta.created_at DESC LIMIT 1) AS ci_head_observation_error
             FROM tasks
             WHERE (status != 'closed' OR closed_at > to_char((now() at time zone 'utc') - interval '1 hour', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
             ORDER BY priority, created_at"#
        };

        if let Some(pid) = project_id {
            Ok(sqlx::query_as::<_, Task>(sql)
                .bind(pid)
                .fetch_all(self.db.pool())
                .await?)
        } else {
            Ok(sqlx::query_as::<_, Task>(sql)
                .fetch_all(self.db.pool())
                .await?)
        }
    }

    /// Upsert a task received from a peer sync (last-writer-wins by updated_at).
    ///
    /// Returns `true` if the row was inserted or updated, `false` if:
    ///   - The task's `epic_id` doesn't exist locally (FK constraint).
    ///   - The local copy is already newer or equal (LWW check).
    ///
    /// On UNIQUE(short_id) constraint violation, the incoming short_id is
    /// extended by one character from the task UUID hex and retried (SYNC-15).
    pub async fn upsert_peer(&self, task: &Task) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        // Verify epic exists before INSERT when task references one.
        if let Some(epic_id) = &task.epic_id {
            let epic_exists: i64 = sqlx::query_scalar!(
                r#"SELECT COUNT(*) AS "count!: i64" FROM epics WHERE id = $1"#,
                epic_id
            )
            .fetch_one(&mut *tx)
            .await?;
            if epic_exists == 0 {
                tx.commit().await?;
                return Ok(false);
            }
        }

        // Pre-check the existing row so we can return a meaningful
        // `changed` flag. `ON DUPLICATE KEY UPDATE` alone isn't a reliable
        // signal on MySQL: sqlx-mysql enables CLIENT_FOUND_ROWS, so any
        // matched row reports rows_affected=1 even when every column is a
        // no-op, including the LWW "this peer is older" and the terminal
        // "local closed, peer wants to regress" paths.
        let existing = sqlx::query!(
            "SELECT updated_at, status FROM tasks WHERE id = $1",
            task.id
        )
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = existing.as_ref() {
            if row.updated_at.as_str() >= task.updated_at.as_str() {
                tx.commit().await?;
                return Ok(false);
            }
            // Terminal state protection: a local `closed` task cannot be
            // regressed by a peer that tries to move it back to a non-closed
            // status. The upsert SQL gates all status-transition columns
            // on the same predicate, so the row is effectively unchanged.
            if row.status == "closed" && task.status != "closed" {
                tx.commit().await?;
                return Ok(false);
            }
        }

        // Peer attribution flows through the transactional provenance
        // boundary: bind the incoming row's creator only when it is known to
        // this instance, so an unreplicated user degrades attribution instead
        // of failing the sync on the users FK.
        let created_by_user_id =
            incoming_task_creator(&mut tx, task.created_by_user_id.as_deref()).await?;

        // Clone task for mutation if we need to extend short_id
        let mut task = task.clone();
        let mut retry_count = 0;
        const MAX_RETRIES: usize = 3;

        let changed = loop {
            // Helper macro: CASE WHEN newer-and-not-undoing-closed THEN excluded ELSE existing.
            // Inlined per-column to keep the SQL readable.
            let result = sqlx::query(
                r#"INSERT INTO tasks (
                    id, project_id, short_id, epic_id, title, description, design,
                    issue_type, status, priority, owner, labels,
                    acceptance_criteria, reopen_count, continuation_count,
                    total_reopen_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs, created_by_user_id
                 ) VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12::jsonb,
                    $13::jsonb, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26::jsonb, $27
                 )
                 ON CONFLICT (id) DO UPDATE SET
                    project_id          = EXCLUDED.project_id,
                    title               = EXCLUDED.title,
                    description         = EXCLUDED.description,
                    design              = EXCLUDED.design,
                    issue_type          = EXCLUDED.issue_type,
                    status              = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.status ELSE tasks.status END,
                    priority            = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.priority ELSE tasks.priority END,
                    owner               = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.owner ELSE tasks.owner END,
                    labels              = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.labels ELSE tasks.labels END,
                    acceptance_criteria = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.acceptance_criteria ELSE tasks.acceptance_criteria END,
                    reopen_count        = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.reopen_count ELSE tasks.reopen_count END,
                    continuation_count  = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.continuation_count ELSE tasks.continuation_count END,
                    total_reopen_count  = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.total_reopen_count ELSE tasks.total_reopen_count END,
                    intervention_count  = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.intervention_count ELSE tasks.intervention_count END,
                    last_intervention_at = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.last_intervention_at ELSE tasks.last_intervention_at END,
                    closed_at           = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.closed_at ELSE tasks.closed_at END,
                    close_reason        = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.close_reason ELSE tasks.close_reason END,
                    merge_commit_sha    = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.merge_commit_sha ELSE tasks.merge_commit_sha END,
                    pr_url              = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.pr_url ELSE tasks.pr_url END,
                    merge_conflict_metadata = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.merge_conflict_metadata ELSE tasks.merge_conflict_metadata END,
                    memory_refs         = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.memory_refs ELSE tasks.memory_refs END,
                    created_by_user_id  = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.created_by_user_id ELSE tasks.created_by_user_id END,
                    updated_at          = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.updated_at ELSE tasks.updated_at END"#,
            )
            .bind(&task.id)
            .bind(&task.project_id)
            .bind(&task.short_id)
            .bind(&task.epic_id)
            .bind(&task.title)
            .bind(&task.description)
            .bind(&task.design)
            .bind(&task.issue_type)
            .bind(&task.status)
            .bind(task.priority)
            .bind(&task.owner)
            .bind(&task.labels)
            .bind(&task.acceptance_criteria)
            .bind(task.reopen_count)
            .bind(task.continuation_count)
            .bind(task.total_reopen_count)
            .bind(task.intervention_count)
            .bind(&task.last_intervention_at)
            .bind(&task.created_at)
            .bind(&task.updated_at)
            .bind(&task.closed_at)
            .bind(&task.close_reason)
            .bind(&task.merge_commit_sha)
            .bind(&task.pr_url)
            .bind(&task.merge_conflict_metadata)
            .bind(&task.memory_refs)
            .bind(&created_by_user_id)
            .execute(&mut *tx)
            .await;

            match result {
                Ok(res) => break res.rows_affected() > 0,
                Err(sqlx::Error::Database(db_err)) if is_constraint_violation(db_err.as_ref()) => {
                    // Check if this is a short_id collision we can handle
                    let constraint_name = extract_constraint_name(db_err.as_ref());

                    if constraint_name.as_deref() == Some("short_id") {
                        retry_count += 1;

                        if retry_count > MAX_RETRIES {
                            warn!(
                                task_id = %task.id,
                                short_id = %task.short_id,
                                retry_count,
                                "Short ID collision retry limit exceeded after {MAX_RETRIES} attempts"
                            );
                            return Err(Error::Sqlx(sqlx::Error::Database(db_err)));
                        }

                        // Get the next character from the UUID hex string
                        let uuid_hex_chars: Vec<char> = task.id.chars().collect();
                        if let Some(next_char) = uuid_hex_chars.get(retry_count - 1) {
                            warn!(
                                task_id = %task.id,
                                short_id = %task.short_id,
                                retry_count,
                                "Short ID collision detected, extending with char '{next_char}'"
                            );
                            task.short_id.push(*next_char);
                        } else {
                            // Shouldn't happen with valid UUIDs, but handle gracefully
                            warn!(
                                task_id = %task.id,
                                short_id = %task.short_id,
                                retry_count,
                                "Short ID collision but UUID exhausted, cannot extend further"
                            );
                            return Err(Error::Sqlx(sqlx::Error::Database(db_err)));
                        }
                        // Continue to next loop iteration with extended short_id
                    } else {
                        // Other constraint violations (FK, etc.) should not be retried
                        warn!(
                            constraint = %db_err.message(),
                            task_id = %task.id,
                            "Non-retriable constraint violation during peer upsert"
                        );
                        return Err(Error::Sqlx(sqlx::Error::Database(db_err)));
                    }
                }
                Err(e) => return Err(Error::Sqlx(e)),
            }
        };

        tx.commit().await?;

        if changed && let Ok(Some(updated)) = self.get(&task.id).await {
            self.events
                .send(DjinnEventEnvelope::task_updated(&updated, true));
        }
        Ok(changed)
    }

    /// Upsert a peer task within an existing transaction (SYNC-10).
    ///
    /// Same logic as `upsert_peer` but executes within the provided transaction
    /// and does NOT emit events. The caller is responsible for emitting events
    /// after the transaction commits.
    ///
    /// Returns `true` if the row was inserted or updated.
    ///
    /// On UNIQUE(short_id) constraint violation, the incoming short_id is
    /// extended by one character from the task UUID hex and retried (SYNC-15).
    pub async fn upsert_peer_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        task: &Task,
    ) -> Result<bool> {
        // Verify epic exists before INSERT when task references one.
        if let Some(epic_id) = &task.epic_id {
            let epic_exists: i64 = sqlx::query_scalar!(
                r#"SELECT COUNT(*) AS "count!: i64" FROM epics WHERE id = $1"#,
                epic_id
            )
            .fetch_one(&mut **tx)
            .await?;
            if epic_exists == 0 {
                return Ok(false);
            }
        }

        // Pre-check existing row; see `upsert_peer` for the CLIENT_FOUND_ROWS
        // rationale and the terminal-state-protection behaviour.
        let existing = sqlx::query!(
            "SELECT updated_at, status FROM tasks WHERE id = $1",
            task.id
        )
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(row) = existing.as_ref() {
            if row.updated_at.as_str() >= task.updated_at.as_str() {
                return Ok(false);
            }
            if row.status == "closed" && task.status != "closed" {
                return Ok(false);
            }
        }

        // Same transactional provenance boundary as `upsert_peer`: only a
        // locally-known incoming creator is bound.
        let created_by_user_id =
            incoming_task_creator(tx, task.created_by_user_id.as_deref()).await?;

        // Clone task for mutation if we need to extend short_id
        let mut task = task.clone();
        let mut retry_count = 0;
        const MAX_RETRIES: usize = 3;

        loop {
            let result = sqlx::query(
                r#"INSERT INTO tasks (
                    id, project_id, short_id, epic_id, title, description, design,
                    issue_type, status, priority, owner, labels,
                    acceptance_criteria, reopen_count, continuation_count,
                    total_reopen_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs, created_by_user_id
                 ) VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12::jsonb,
                    $13::jsonb, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26::jsonb, $27
                 )
                 ON CONFLICT (id) DO UPDATE SET
                    project_id          = EXCLUDED.project_id,
                    title               = EXCLUDED.title,
                    description         = EXCLUDED.description,
                    design              = EXCLUDED.design,
                    issue_type          = EXCLUDED.issue_type,
                    status              = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.status ELSE tasks.status END,
                    priority            = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.priority ELSE tasks.priority END,
                    owner               = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.owner ELSE tasks.owner END,
                    labels              = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.labels ELSE tasks.labels END,
                    acceptance_criteria = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.acceptance_criteria ELSE tasks.acceptance_criteria END,
                    reopen_count        = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.reopen_count ELSE tasks.reopen_count END,
                    continuation_count  = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.continuation_count ELSE tasks.continuation_count END,
                    total_reopen_count  = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.total_reopen_count ELSE tasks.total_reopen_count END,
                    intervention_count  = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.intervention_count ELSE tasks.intervention_count END,
                    last_intervention_at = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.last_intervention_at ELSE tasks.last_intervention_at END,
                    closed_at           = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.closed_at ELSE tasks.closed_at END,
                    close_reason        = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.close_reason ELSE tasks.close_reason END,
                    merge_commit_sha    = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.merge_commit_sha ELSE tasks.merge_commit_sha END,
                    pr_url              = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.pr_url ELSE tasks.pr_url END,
                    created_by_user_id  = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.created_by_user_id ELSE tasks.created_by_user_id END,
                    merge_conflict_metadata = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.merge_conflict_metadata ELSE tasks.merge_conflict_metadata END,
                    memory_refs         = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.memory_refs ELSE tasks.memory_refs END,
                    updated_at          = CASE WHEN EXCLUDED.updated_at > tasks.updated_at AND NOT (tasks.status = 'closed' AND EXCLUDED.status != 'closed') THEN EXCLUDED.updated_at ELSE tasks.updated_at END"#,
            )
            .bind(&task.id)
            .bind(&task.project_id)
            .bind(&task.short_id)
            .bind(&task.epic_id)
            .bind(&task.title)
            .bind(&task.description)
            .bind(&task.design)
            .bind(&task.issue_type)
            .bind(&task.status)
            .bind(task.priority)
            .bind(&task.owner)
            .bind(&task.labels)
            .bind(&task.acceptance_criteria)
            .bind(task.reopen_count)
            .bind(task.continuation_count)
            .bind(task.total_reopen_count)
            .bind(task.intervention_count)
            .bind(&task.last_intervention_at)
            .bind(&task.created_at)
            .bind(&task.updated_at)
            .bind(&task.closed_at)
            .bind(&task.close_reason)
            .bind(&task.merge_commit_sha)
            .bind(&task.pr_url)
            .bind(&task.merge_conflict_metadata)
            .bind(&task.memory_refs)
            .bind(&created_by_user_id)
            .execute(&mut **tx)
            .await;

            match result {
                Ok(res) => return Ok(res.rows_affected() > 0),
                Err(sqlx::Error::Database(db_err)) if is_constraint_violation(db_err.as_ref()) => {
                    // Check if this is a short_id collision we can handle
                    let constraint_name = extract_constraint_name(db_err.as_ref());

                    if constraint_name.as_deref() == Some("short_id") {
                        retry_count += 1;

                        if retry_count > MAX_RETRIES {
                            warn!(
                                task_id = %task.id,
                                short_id = %task.short_id,
                                retry_count,
                                "Short ID collision retry limit exceeded after {MAX_RETRIES} attempts"
                            );
                            return Err(Error::Sqlx(sqlx::Error::Database(db_err)));
                        }

                        // Get the next character from the UUID hex string
                        let uuid_hex_chars: Vec<char> = task.id.chars().collect();
                        if let Some(next_char) = uuid_hex_chars.get(retry_count - 1) {
                            warn!(
                                task_id = %task.id,
                                short_id = %task.short_id,
                                retry_count,
                                "Short ID collision detected, extending with char '{next_char}'"
                            );
                            task.short_id.push(*next_char);
                        } else {
                            // Shouldn't happen with valid UUIDs, but handle gracefully
                            warn!(
                                task_id = %task.id,
                                short_id = %task.short_id,
                                retry_count,
                                "Short ID collision but UUID exhausted, cannot extend further"
                            );
                            return Err(Error::Sqlx(sqlx::Error::Database(db_err)));
                        }
                        // Continue to next loop iteration with extended short_id
                    } else {
                        // Other constraint violations (FK, etc.) should not be retried
                        warn!(
                            constraint = %db_err.message(),
                            task_id = %task.id,
                            "Non-retriable constraint violation during peer upsert"
                        );
                        return Err(Error::Sqlx(sqlx::Error::Database(db_err)));
                    }
                }
                Err(e) => return Err(Error::Sqlx(e)),
            }
        }
    }

    /// Reconciles tasks for a specific peer within a transaction.
    ///
    /// - Finds tasks where owner = peer_user_id
    /// - Skips already-closed tasks (terminal state protection - SYNC-11)
    /// - Skips tasks whose IDs are in peer_task_ids
    /// - Closes remaining tasks with close_reason = 'peer_reconciled'
    ///
    /// Returns the count of tasks that were reconciled (closed).
    pub async fn reconcile_peer_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        peer_user_id: &str,
        peer_task_ids: &[String],
    ) -> Result<usize> {
        // Safety guard: if peer export is empty, skip reconciliation
        if peer_task_ids.is_empty() {
            return Ok(0);
        }

        // NOT IN placeholders: peer_user_id is bound as $1, so the list
        // starts at $2 (Postgres positional binds; `?` is a syntax error).
        let placeholders = crate::repositories::pg_placeholders(peer_task_ids.len(), 2);

        // Find tasks owned by peer that are not in their export and not already closed
        let sql_select = format!(
            "SELECT id FROM tasks WHERE owner = $1 AND status != 'closed' AND id NOT IN ({})",
            placeholders
        );

        let mut query = sqlx::query_scalar::<_, String>(&sql_select).bind(peer_user_id);
        for id in peer_task_ids {
            query = query.bind(id);
        }

        let tasks_to_close: Vec<String> = query.fetch_all(&mut **tx).await?;

        if tasks_to_close.is_empty() {
            return Ok(0);
        }

        // IN placeholders: no fixed param precedes the list, so start at $1.
        let placeholders = crate::repositories::pg_placeholders(tasks_to_close.len(), 1);

        let sql_update = format!(
            r#"UPDATE tasks SET status = 'closed', close_reason = 'peer_reconciled',
             closed_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') WHERE id IN ({})"#,
            placeholders
        );

        let mut update_query = sqlx::query(&sql_update);
        for id in &tasks_to_close {
            update_query = update_query.bind(id);
        }

        let result = update_query.execute(&mut **tx).await?;

        Ok(result.rows_affected() as usize)
    }

    /// Read only the `created_by_user_id` column for a single task.
    ///
    /// Returns `None` when the task is not found or when the column is NULL
    /// (background-agent-created tasks).  Errors are propagated.
    pub async fn created_by_user_id(&self, task_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let row: Option<Option<String>> =
            sqlx::query_scalar("SELECT created_by_user_id FROM tasks WHERE id = $1")
                .bind(task_id)
                .fetch_optional(self.db.pool())
                .await?;
        // Flatten: outer None = no row, inner None = NULL column value.
        Ok(row.flatten())
    }

    /// Count exact-title matches for integration tests that verify transactional
    /// task insertion rollback without issuing SQL outside the repository layer.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn count_by_title(&self, title: &str) -> Result<i64> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE title = $1")
                .bind(title)
                .fetch_one(self.db.pool())
                .await?,
        )
    }
}
