// Prompt amendment keep/discard evaluation (task svox).
//
// After N tasks complete post-amendment (configurable, default 10):
//   - Compare pre-amendment window metrics vs post-amendment window metrics.
//   - Keep criteria: success rate improved ≥5% OR token usage decreased ≥10%
//     without success regression.
//   - Discard: otherwise.
//
// The evaluation runs as part of the coordinator tick (every 30 s), but is
// rate-limited to once per prune tick (~1 hour) to avoid DB churn.

use super::*;
use djinn_db::{AgentRoleRepository, PendingAmendmentEvaluation, WindowedRoleMetrics};

/// Number of tasks that must complete after an amendment before evaluation
/// is triggered.  Configurable; default is 10.
pub(super) const DEFAULT_EVAL_TASK_COUNT: i64 = 10;

/// Minimum improvement in success rate (absolute, 0.0–1.0) to qualify as
/// statistically meaningful.  5% = 0.05.
const SUCCESS_RATE_IMPROVEMENT_THRESHOLD: f64 = 0.05;

/// Minimum decrease in avg tokens (relative) to qualify as meaningful.  10% = 0.10.
const TOKEN_DECREASE_THRESHOLD: f64 = 0.10;

/// Map a `base_role` to the session `agent_type` string used by sessions.
fn base_role_to_agent_type(base_role: &str) -> &str {
    match base_role {
        "worker" | "resolver" => "worker",
        "reviewer" => "reviewer",
        "planner" => "planner",
        "lead" => "lead",
        other => other,
    }
}

/// Decision from comparing two metric windows.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum EvalDecision {
    /// Metrics improved meaningfully — keep the amendment.
    Confirmed,
    /// Metrics did not improve — discard/revert the amendment.
    Discard,
    /// Not enough post-amendment tasks yet — skip for now.
    NotReady,
}

/// Compare pre-amendment and post-amendment metrics and return a decision.
///
/// This function is pure (no I/O) and fully testable.
pub(super) fn decide(
    pre: &WindowedRoleMetrics,
    post: &WindowedRoleMetrics,
    min_post_tasks: i64,
) -> EvalDecision {
    if post.completed_task_count + post.failed_task_count < min_post_tasks {
        return EvalDecision::NotReady;
    }

    let success_delta = post.success_rate - pre.success_rate;
    let token_delta_ratio = if pre.avg_tokens > 0.0 {
        (pre.avg_tokens - post.avg_tokens) / pre.avg_tokens
    } else {
        0.0
    };

    // Keep if success rate improved meaningfully.
    if success_delta >= SUCCESS_RATE_IMPROVEMENT_THRESHOLD {
        return EvalDecision::Confirmed;
    }

    // Keep if tokens decreased meaningfully without success regression.
    if token_delta_ratio >= TOKEN_DECREASE_THRESHOLD
        && success_delta >= -SUCCESS_RATE_IMPROVEMENT_THRESHOLD
    {
        return EvalDecision::Confirmed;
    }

    // Otherwise discard.
    EvalDecision::Discard
}

impl CoordinatorActor {
    /// Evaluate pending prompt amendments for all projects.
    ///
    /// For each role with a pending 'keep' entry in `learned_prompt_history`
    /// (metrics_after IS NULL):
    ///   1. Count tasks completed since the amendment.
    ///   2. If fewer than N tasks have completed, skip (not ready).
    ///   3. Fetch pre-window and post-window metrics.
    ///   4. Apply keep/discard logic.
    ///   5. Update the history record; if discard, revert learned_prompt.
    pub(super) async fn evaluate_prompt_amendments(&self) {
        let role_repo = AgentRoleRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );

        let project_repo = djinn_db::ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let projects = match project_repo.list().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: prompt eval — failed to list projects");
                return;
            }
        };

        for project in projects {
            let pending = match role_repo.get_pending_evaluations(&project.id).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        project_id = %project.id,
                        error = %e,
                        "CoordinatorActor: prompt eval — failed to get pending evaluations"
                    );
                    continue;
                }
            };

            for amendment in pending {
                self.evaluate_one_amendment(&role_repo, &project.id, &amendment)
                    .await;
            }
        }
    }

    async fn evaluate_one_amendment(
        &self,
        role_repo: &AgentRoleRepository,
        project_id: &str,
        amendment: &PendingAmendmentEvaluation,
    ) {
        // Fetch the role to get base_role → agent_type mapping.
        let role = match role_repo.get(&amendment.role_id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                tracing::warn!(
                    role_id = %amendment.role_id,
                    "CoordinatorActor: prompt eval — role not found, skipping"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    role_id = %amendment.role_id,
                    error = %e,
                    "CoordinatorActor: prompt eval — failed to load role"
                );
                return;
            }
        };

        let agent_type = base_role_to_agent_type(&role.base_role);

        // Count tasks completed since the amendment was created.
        let (completed_since, failed_since) = match role_repo
            .count_closed_tasks_since(project_id, agent_type, &amendment.created_at)
            .await
        {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(
                    role_id = %amendment.role_id,
                    error = %e,
                    "CoordinatorActor: prompt eval — failed to count tasks since amendment"
                );
                return;
            }
        };

        let post_total = completed_since + failed_since;
        if post_total < DEFAULT_EVAL_TASK_COUNT {
            tracing::debug!(
                role_id = %amendment.role_id,
                post_total,
                needed = DEFAULT_EVAL_TASK_COUNT,
                "CoordinatorActor: prompt eval — not enough post-amendment tasks yet"
            );
            return;
        }

        // Fetch post-amendment metrics (tasks closed after amendment timestamp).
        let post_metrics = match role_repo
            .get_windowed_metrics(project_id, agent_type, Some(&amendment.created_at), None)
            .await
        {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    role_id = %amendment.role_id,
                    error = %e,
                    "CoordinatorActor: prompt eval — failed to get post-window metrics"
                );
                return;
            }
        };

        // Decode pre-amendment metrics from the JSON snapshot stored at proposal time.
        // If not available, create a zero baseline (will likely result in discard
        // unless post metrics show genuine improvement over empty pre-window).
        let pre_metrics = parse_metrics_snapshot(amendment.metrics_before.as_deref());

        let decision = decide(&pre_metrics, &post_metrics, DEFAULT_EVAL_TASK_COUNT);
        if decision == EvalDecision::NotReady {
            return;
        }

        let action = match decision {
            EvalDecision::Confirmed => "confirmed",
            EvalDecision::Discard => "discard",
            EvalDecision::NotReady => return,
        };

        let metrics_after_json = serde_json::json!({
            "success_rate": post_metrics.success_rate,
            "avg_tokens": post_metrics.avg_tokens,
            "completed_task_count": post_metrics.completed_task_count,
            "failed_task_count": post_metrics.failed_task_count,
        })
        .to_string();

        // Update history record.
        if let Err(e) = role_repo
            .resolve_pending_amendment(&amendment.history_id, action, &metrics_after_json)
            .await
        {
            tracing::warn!(
                history_id = %amendment.history_id,
                error = %e,
                "CoordinatorActor: prompt eval — failed to update history record"
            );
            return;
        }

        // If discarding: revert the learned_prompt.
        if action == "discard" {
            match role_repo
                .revert_learned_prompt(&amendment.role_id, &amendment.proposed_text)
                .await
            {
                Ok(_) => {
                    tracing::info!(
                        role_id = %amendment.role_id,
                        history_id = %amendment.history_id,
                        pre_success_rate = pre_metrics.success_rate,
                        post_success_rate = post_metrics.success_rate,
                        pre_avg_tokens = pre_metrics.avg_tokens,
                        post_avg_tokens = post_metrics.avg_tokens,
                        "CoordinatorActor: prompt eval — amendment discarded, learned_prompt reverted"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        role_id = %amendment.role_id,
                        error = %e,
                        "CoordinatorActor: prompt eval — failed to revert learned_prompt"
                    );
                }
            }
        } else {
            tracing::info!(
                role_id = %amendment.role_id,
                history_id = %amendment.history_id,
                pre_success_rate = pre_metrics.success_rate,
                post_success_rate = post_metrics.success_rate,
                pre_avg_tokens = pre_metrics.avg_tokens,
                post_avg_tokens = post_metrics.avg_tokens,
                "CoordinatorActor: prompt eval — amendment confirmed"
            );
        }
    }
}

/// Parse a metrics snapshot JSON string into `WindowedRoleMetrics`.
/// Returns a zero-baseline on any parse failure.
fn parse_metrics_snapshot(snapshot: Option<&str>) -> WindowedRoleMetrics {
    let Some(json) = snapshot else {
        return WindowedRoleMetrics {
            completed_task_count: 0,
            failed_task_count: 0,
            success_rate: 0.0,
            avg_tokens: 0.0,
        };
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return WindowedRoleMetrics {
            completed_task_count: 0,
            failed_task_count: 0,
            success_rate: 0.0,
            avg_tokens: 0.0,
        };
    };
    let success_rate = value
        .get("success_rate")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let avg_tokens = value
        .get("avg_tokens")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let completed_task_count = value
        .get("completed_task_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let failed_task_count = value
        .get("failed_task_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    WindowedRoleMetrics {
        completed_task_count,
        failed_task_count,
        success_rate,
        avg_tokens,
    }
}

// ── Unit tests for keep/discard decision logic ────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::WindowedRoleMetrics;

    fn metrics(success_rate: f64, avg_tokens: f64, completed: i64) -> WindowedRoleMetrics {
        WindowedRoleMetrics {
            completed_task_count: completed,
            failed_task_count: 0,
            success_rate,
            avg_tokens,
        }
    }

    fn metrics_with_failed(
        success_rate: f64,
        avg_tokens: f64,
        completed: i64,
        failed: i64,
    ) -> WindowedRoleMetrics {
        WindowedRoleMetrics {
            completed_task_count: completed,
            failed_task_count: failed,
            success_rate,
            avg_tokens,
        }
    }

    // ── NotReady ──────────────────────────────────────────────────────────────

    #[test]
    fn not_ready_when_post_tasks_below_threshold() {
        let pre = metrics(0.7, 1000.0, 10);
        let post = metrics_with_failed(0.8, 900.0, 5, 4); // total = 9 < 10
        assert_eq!(decide(&pre, &post, 10), EvalDecision::NotReady);
    }

    #[test]
    fn ready_when_exactly_at_threshold() {
        let pre = metrics(0.7, 1000.0, 10);
        let post = metrics_with_failed(0.8, 900.0, 10, 0); // total = 10 >= 10
        assert_eq!(decide(&pre, &post, 10), EvalDecision::Confirmed);
    }

    // ── Success rate improvement → Confirmed ──────────────────────────────────

    #[test]
    fn success_rate_improved_by_5pct_confirms() {
        let pre = metrics(0.70, 1000.0, 10);
        let post = metrics(0.75, 1000.0, 10); // +5%
        assert_eq!(decide(&pre, &post, 10), EvalDecision::Confirmed);
    }

    #[test]
    fn success_rate_improved_more_than_5pct_confirms() {
        let pre = metrics(0.50, 1000.0, 10);
        let post = metrics(0.80, 1000.0, 10); // +30%
        assert_eq!(decide(&pre, &post, 10), EvalDecision::Confirmed);
    }

    #[test]
    fn success_rate_improved_less_than_5pct_no_token_change_discards() {
        let pre = metrics(0.70, 1000.0, 10);
        let post = metrics(0.74, 1000.0, 10); // +4% — below threshold
        assert_eq!(decide(&pre, &post, 10), EvalDecision::Discard);
    }

    // ── Token decrease without success regression → Confirmed ─────────────────

    #[test]
    fn token_decrease_10pct_with_no_regression_confirms() {
        let pre = metrics(0.70, 1000.0, 10);
        let post = metrics(0.70, 900.0, 10); // tokens -10%, success unchanged
        assert_eq!(decide(&pre, &post, 10), EvalDecision::Confirmed);
    }

    #[test]
    fn token_decrease_10pct_with_minor_success_regression_discards() {
        let pre = metrics(0.70, 1000.0, 10);
        // tokens -10%, but success -6% which exceeds the -5% tolerance
        let post = metrics(0.64, 900.0, 10);
        assert_eq!(decide(&pre, &post, 10), EvalDecision::Discard);
    }

    #[test]
    fn token_decrease_less_than_10pct_discards() {
        let pre = metrics(0.70, 1000.0, 10);
        let post = metrics(0.70, 920.0, 10); // tokens -8% — below 10% threshold
        assert_eq!(decide(&pre, &post, 10), EvalDecision::Discard);
    }

    // ── Success regression → Discard ──────────────────────────────────────────

    #[test]
    fn success_regression_discards() {
        let pre = metrics(0.80, 1000.0, 10);
        let post = metrics(0.70, 800.0, 10); // tokens improved but success dropped
        assert_eq!(decide(&pre, &post, 10), EvalDecision::Discard);
    }

    // ── No change → Discard ───────────────────────────────────────────────────

    #[test]
    fn no_change_discards() {
        let pre = metrics(0.75, 1000.0, 10);
        let post = metrics(0.75, 1000.0, 10);
        assert_eq!(decide(&pre, &post, 10), EvalDecision::Discard);
    }

    // ── Zero pre-window metrics (first amendment ever) ────────────────────────

    #[test]
    fn zero_pre_metrics_success_above_zero_confirms() {
        let pre = metrics(0.0, 0.0, 0);
        let post = metrics(0.80, 500.0, 10); // success jumped from 0
        assert_eq!(decide(&pre, &post, 10), EvalDecision::Confirmed);
    }

    // ── parse_metrics_snapshot ────────────────────────────────────────────────

    #[test]
    fn parse_metrics_none_returns_zero_baseline() {
        let m = parse_metrics_snapshot(None);
        assert_eq!(m.success_rate, 0.0);
        assert_eq!(m.avg_tokens, 0.0);
        assert_eq!(m.completed_task_count, 0);
    }

    #[test]
    fn parse_metrics_valid_json_round_trips() {
        let json = serde_json::json!({
            "success_rate": 0.85,
            "avg_tokens": 1234.5,
            "completed_task_count": 20,
            "failed_task_count": 3,
        })
        .to_string();
        let m = parse_metrics_snapshot(Some(&json));
        assert!((m.success_rate - 0.85).abs() < 1e-9);
        assert!((m.avg_tokens - 1234.5).abs() < 1e-9);
        assert_eq!(m.completed_task_count, 20);
        assert_eq!(m.failed_task_count, 3);
    }

    #[test]
    fn parse_metrics_invalid_json_returns_zero_baseline() {
        let m = parse_metrics_snapshot(Some("not-valid-json"));
        assert_eq!(m.success_rate, 0.0);
        assert_eq!(m.avg_tokens, 0.0);
    }
}
