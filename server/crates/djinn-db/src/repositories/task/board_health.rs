//! Board-health JSON sections that are large enough to warrant living outside
//! `queries.rs` (keeping that file under the repository size guard).
//!
//! These builders back `TaskRepository::board_health`. Each returns an additive
//! JSON object that is spliced into the top-level board-health report:
//!   * [`liveness_outcomes_section`]  — bounded recent liveness-classifier evidence.
//!   * [`protocol_violations_section`] — bounded recent protocol-violation evidence.
//!   * [`stranded_ready_section`]     — ready/dispatchable tasks that are not being
//!     picked up, with dispatch-gate evidence and exclusion of tasks whose
//!     non-dispatch is already explained by a visible gate (breaker cooldown,
//!     rate-limit backoff, manual pause, or revoked owner credentials).
//!
//! Every query here uses runtime `sqlx::query`/`query_scalar` (not the
//! compile-time macros) so the sections stay resilient to schema evolution and
//! add no entries to the offline query cache.

use std::collections::HashMap;

use sqlx::Row;

/// Bounded number of recent liveness-evidence rows surfaced in `liveness_outcomes`.
const LIVENESS_OUTCOMES_LIMIT: i64 = 25;
/// Bounded number of recent protocol-violation rows surfaced.
const PROTOCOL_VIOLATIONS_LIMIT: i64 = 15;
/// Base stranded-ready threshold: a ready task unclaimed for this many minutes
/// is `warning`; ≥2× is `error`; ≥6× is `critical`.
const STRANDED_THRESHOLD_MINUTES: i64 = 30;

/// Model-health statuses that indicate the chosen model is NOT dispatchable.
/// Any other status (including the unprobed default `unknown`) is treated as
/// ready so board_health does not raise false gate alarms.
const UNHEALTHY_MODEL_STATUSES: &[&str] = &[
    "unreachable",
    "error",
    "down",
    "offline",
    "disconnected",
    "unhealthy",
];

/// Compute the number of elapsed minutes between two ISO-8601 UTC timestamps
/// stored as `YYYY-MM-DDTHH:MM:SS.MSZ`. Returns `None` if either timestamp
/// cannot be parsed. This is intentionally minimal — the format is always
/// produced by Postgres `to_char(now(), ...)` and validated at insert time.
pub(super) fn elapsed_minutes_iso(iso_start: &str, iso_end: &str) -> Option<i64> {
    fn parse_iso_minutes(s: &str) -> Option<i64> {
        // Expected: "2025-01-15T10:30:00.000Z"
        if s.len() < 20 || !s.is_ascii() {
            return None;
        }
        let bytes = s.as_bytes();
        if bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
            return None;
        }
        let year: i64 = s[0..4].parse().ok()?;
        let month: u32 = s[5..7].parse().ok()?;
        let day: u32 = s[8..10].parse().ok()?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        // Find the seconds end: the format is HH:MM:SS.MSZ so we look for '.'
        // (fractional seconds start) or 'Z' (no fractional part fallback).
        let time_part = &s[11..];
        let sec_end = time_part
            .find('.')
            .or_else(|| time_part.find('Z'))
            .unwrap_or(time_part.len());
        // There must be at least HH:MM:SS (8 chars)
        if sec_end < 8 {
            return None;
        }
        let seconds_str = &time_part[0..sec_end];
        // Parse HH:MM:SS
        let total_seconds: i64 =
            seconds_str
                .split(':')
                .enumerate()
                .try_fold(0i64, |acc, (i, part)| {
                    let v: i64 = part.parse().ok()?;
                    Some(acc + v * [3600, 60, 1][i])
                })?;
        // Total days since year 0 (proleptic Gregorian, good enough for elapsed
        // computation — we only care about the *difference* between two nearby
        // timestamps, so leap-year drift is negligible).
        let is_leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let month_days: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
        let mut doy = month_days[(month - 1) as usize] + (day as i64 - 1);
        if month > 2 && is_leap {
            doy += 1;
        }
        let total_days = year * 365 + year / 4 - year / 100 + year / 400 + doy;
        Some(total_days * 24 * 60 + total_seconds / 60)
    }
    let start = parse_iso_minutes(iso_start)?;
    let end = parse_iso_minutes(iso_end)?;
    Some(end - start)
}

/// Return the current UTC time formatted as ISO-8601 with milliseconds from the
/// database clock, matching the Postgres `to_char(now() ...)` format used
/// throughout the codebase. Used for string-comparison thresholds.
pub(super) async fn db_utc_now(pool: &sqlx::PgPool) -> String {
    sqlx::query_scalar(
        r#"SELECT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#,
    )
    .fetch_one(pool)
    .await
    .unwrap_or_else(|_| "9999-12-31T23:59:59.999Z".to_string())
}

/// Derive the role the coordinator would evaluate for a task given only its
/// persisted `status` and `issue_type`. Mirrors `dispatched_role_for_task`,
/// which needs a full `Task`; board_health only has row columns.
fn dispatched_role_for_status_type(status: &str, issue_type: &str) -> &'static str {
    match status {
        "needs_task_review" | "in_task_review" => "reviewer",
        "needs_lead_intervention" | "in_lead_intervention" => "lead",
        _ => match issue_type {
            "planning" | "decomposition" => "planner",
            "spike" | "review" => "architect",
            _ => "worker",
        },
    }
}

/// Bounded recent liveness-classifier evidence.
pub(super) async fn liveness_outcomes_section(pool: &sqlx::PgPool) -> serde_json::Value {
    let rows = sqlx::query(
        r#"SELECT le.verdict, le.outcome_kind, le.outcome_reason,
                  le.created_at, le.task_id, le.session_id
           FROM liveness_evidence le
           ORDER BY le.created_at DESC
           LIMIT $1"#,
    )
    .bind(LIVENESS_OUTCOMES_LIMIT)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let total = rows.len();
    let by_verdict = {
        let mut counts = serde_json::Map::new();
        for row in &rows {
            let v: String = row.get("verdict");
            let entry = counts.entry(v).or_insert(serde_json::json!(0));
            *entry = serde_json::json!(entry.as_i64().unwrap_or(0) + 1);
        }
        serde_json::Value::Object(counts)
    };
    let recent: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "verdict":        row.get::<String, _>("verdict"),
                "outcome_kind":   row.get::<Option<String>, _>("outcome_kind"),
                "outcome_reason": row.get::<Option<String>, _>("outcome_reason"),
                "created_at":     row.get::<String, _>("created_at"),
                "task_id":        row.get::<Option<String>, _>("task_id"),
                "session_id":     row.get::<String, _>("session_id"),
            })
        })
        .collect();

    serde_json::json!({
        "total":      total,
        "by_verdict": by_verdict,
        "recent":     recent,
    })
}

/// Bounded recent protocol-violation evidence.
pub(super) async fn protocol_violations_section(pool: &sqlx::PgPool) -> serde_json::Value {
    let rows = sqlx::query(
        r#"SELECT le.verdict, le.outcome_kind, le.outcome_reason,
                  le.created_at, le.task_id, le.session_id,
                  t.short_id AS task_short_id,
                  t.title    AS task_title,
                  t.status   AS task_status
           FROM liveness_evidence le
           LEFT JOIN tasks t ON t.id = le.task_id
           WHERE le.verdict = 'protocol_violation'
              OR le.outcome_kind = 'protocol_violation'
           ORDER BY le.created_at DESC
           LIMIT $1"#,
    )
    .bind(PROTOCOL_VIOLATIONS_LIMIT)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let total = rows.len();
    let recent: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "verdict":        row.get::<String, _>("verdict"),
                "outcome_kind":   row.get::<Option<String>, _>("outcome_kind"),
                "outcome_reason": row.get::<Option<String>, _>("outcome_reason"),
                "created_at":     row.get::<String, _>("created_at"),
                "task_id":        row.get::<Option<String>, _>("task_id"),
                "session_id":     row.get::<String, _>("session_id"),
                "task_short_id":  row.get::<Option<String>, _>("task_short_id"),
                "task_title":     row.get::<Option<String>, _>("task_title"),
                "task_status":    row.get::<Option<String>, _>("task_status"),
            })
        })
        .collect();

    serde_json::json!({
        "total":  total,
        "recent": recent,
    })
}

/// Ready/dispatchable tasks with no active running session whose unclaimed
/// duration exceeds the stranded threshold, each annotated with dispatch-gate
/// evidence.
///
/// Tasks whose non-dispatch is already explained by a visible gate are
/// **excluded** so the findings surface genuine dispatch starvation rather than
/// intentionally-held work:
///   * breaker-open (a future `cooldown_until`),
///   * rate-limit backoff (`failure_streak >= 3`),
///   * manual pause (a `paused` session or a project/user `dispatch_pauses` row),
///   * owner-credential-blocked (the creator has credentials but they are all
///     revoked and no org-shared fallback credential is available).
///
/// Surviving findings carry a `dispatch_gate` object documenting the evaluated
/// role, its toolset, the model requirement, image readiness, each gate flag,
/// credential availability, a final `gate_verdict`, and machine-readable
/// `reasons` (e.g. `no_eligible_model`, `image_not_ready`).
pub(super) async fn stranded_ready_section(pool: &sqlx::PgPool) -> serde_json::Value {
    let warning_threshold = STRANDED_THRESHOLD_MINUTES;
    let error_threshold = STRANDED_THRESHOLD_MINUTES * 2;
    let critical_threshold = STRANDED_THRESHOLD_MINUTES * 6;

    // Prefetch the model-health rollup keyed by full `provider/model` id so a
    // task's inflight model can be classified without an N+1 join.
    let model_health: HashMap<String, String> =
        sqlx::query(r#"SELECT provider || '/' || model AS model_id, status FROM model_health"#)
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| {
                let id: Option<String> = row.try_get("model_id").ok().flatten();
                let status: String = row.try_get("status").ok()?;
                id.map(|id| (id, status))
            })
            .collect();

    let now_iso = db_utc_now(pool).await;

    let stranded_sql = r#"SELECT t.id, t.short_id, t.title, t.status, t.updated_at, t.owner,
                  t.epic_id, t.issue_type, t.project_id, t.created_by_user_id,
                  e.short_id AS epic_short_id,
                  ds.last_dispatched_role,
                  to_char(ds.cooldown_until AT TIME ZONE 'utc',
                          'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS cooldown_until,
                  COALESCE(ds.failure_streak, 0)::BIGINT AS failure_streak,
                  ds.inflight_model_id,
                  (SELECT al.created_at
                   FROM activity_log al
                   WHERE al.task_id = t.id
                     AND al.event_type = 'status_changed'
                     AND al.payload->>'to_status' = 'open'
                   ORDER BY al.created_at DESC
                   LIMIT 1) AS open_transition_at,
                  (SELECT s.ended_at
                   FROM sessions s
                   WHERE s.task_id = t.id AND s.ended_at IS NOT NULL
                   ORDER BY s.ended_at DESC
                   LIMIT 1) AS session_release_at,
                  EXISTS (SELECT 1 FROM sessions s
                          WHERE s.task_id = t.id AND s.status = 'paused') AS has_paused_session,
                  EXISTS (SELECT 1 FROM dispatch_pauses dp
                          WHERE ((dp.scope = 'project' AND dp.target_id = t.project_id)
                              OR (dp.scope = 'user'    AND dp.target_id = t.created_by_user_id))
                            AND (dp.expires_at IS NULL
                                 OR dp.expires_at > to_char(now() AT TIME ZONE 'utc',
                                                            'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                         ) AS dispatch_paused,
                  EXISTS (SELECT 1 FROM credentials c
                          WHERE c.revoked_at IS NULL
                            AND (c.owner_user_id = t.created_by_user_id
                                 OR c.owner_user_id IS NULL)
                         ) AS has_active_credential,
                  EXISTS (SELECT 1 FROM credentials c
                          WHERE c.owner_user_id = t.created_by_user_id
                         ) AS has_owner_credential
           FROM tasks t
           LEFT JOIN epics e ON e.id = t.epic_id
           LEFT JOIN dispatch_state ds ON ds.task_id = t.id
           WHERE t.status IN ('open', 'in_progress')
             -- Redundant with the IN filter above, but defensive against
             -- terminal/review statuses.
             AND t.status NOT IN ('closed', 'needs_task_review',
                                  'in_task_review', 'approved',
                                  'pr_draft', 'pr_review')
             -- No active running session for this task.
             AND NOT EXISTS (
                 SELECT 1 FROM sessions s
                 WHERE s.task_id = t.id AND s.status = 'running'
             )
             -- Exclude blocked tasks (unresolved blockers).
             AND NOT EXISTS (
                 SELECT 1 FROM blockers b
                 JOIN tasks bt ON b.blocking_task_id = bt.id
                 WHERE b.task_id = t.id AND bt.status != 'closed'
             )
           ORDER BY t.updated_at ASC"#;

    let rows = sqlx::query(stranded_sql)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

    let findings: Vec<serde_json::Value> = rows
        .into_iter()
        .filter_map(|row| {
            let task_status: String = row.get("status");
            let issue_type: String = row.get("issue_type");

            // ── Exclusion gates ────────────────────────────────────────────
            // Each of these explains, from visible DB state, why the task is
            // (correctly) not being dispatched, so it is not "stranded".

            // Breaker-open: a future cooldown deadline.
            let cooldown_until: Option<String> = row.try_get("cooldown_until").ok().flatten();
            let breaker_open = cooldown_until
                .as_deref()
                .is_some_and(|cd| cd > now_iso.as_str());
            if breaker_open {
                return None;
            }

            // Rate-limit backoff: the dispatch backoff ladder has tripped.
            let failure_streak: i64 = row.try_get("failure_streak").unwrap_or(0);
            let rate_limited = failure_streak >= 3;
            if rate_limited {
                return None;
            }

            // Manual pause: a paused session or a project/user dispatch pause.
            let has_paused_session: bool = row.try_get("has_paused_session").unwrap_or(false);
            let dispatch_paused: bool = row.try_get("dispatch_paused").unwrap_or(false);
            let manually_paused = has_paused_session || dispatch_paused;
            if manually_paused {
                return None;
            }

            // Owner-credential-blocked: the creator's credentials are all
            // revoked and no org-shared fallback is available. Tasks with no
            // creator or no credential rows are NOT treated as blocked.
            let created_by: Option<String> = row.try_get("created_by_user_id").ok().flatten();
            let has_active_credential: bool = row.try_get("has_active_credential").unwrap_or(false);
            let has_owner_credential: bool = row.try_get("has_owner_credential").unwrap_or(false);
            let credential_available = created_by.is_none() || has_active_credential;
            let credential_blocked =
                created_by.is_some() && !has_active_credential && has_owner_credential;
            if credential_blocked {
                return None;
            }

            // ── Unclaimed-since / severity ─────────────────────────────────
            // Prefer the most recent high-confidence release signal (ready/open
            // transition or session release); fall back to updated_at.
            let open_transition_at: Option<String> =
                row.try_get("open_transition_at").ok().flatten();
            let session_release_at: Option<String> =
                row.try_get("session_release_at").ok().flatten();
            let updated_at: String = row.get("updated_at");

            let high_signal = match (open_transition_at, session_release_at) {
                (Some(a), Some(b)) => Some(if a >= b { a } else { b }),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            let (unclaimed_since, confidence) = match high_signal {
                Some(ts) => (ts, "high"),
                None => (updated_at.clone(), "low"),
            };

            let elapsed = elapsed_minutes_iso(&unclaimed_since, &now_iso).unwrap_or(0);
            // Not yet stranded (below 1× threshold).
            if elapsed < warning_threshold {
                return None;
            }
            let severity = if elapsed >= critical_threshold {
                "critical"
            } else if elapsed >= error_threshold {
                "error"
            } else {
                "warning"
            };

            // ── Model / image readiness evidence ───────────────────────────
            let inflight_model_id: Option<String> = row.try_get("inflight_model_id").ok().flatten();
            let mut reasons: Vec<&str> = Vec::new();
            let image_ready = match inflight_model_id.as_deref() {
                // No model chosen yet — nothing model-specific to block on.
                None => true,
                Some(mid) => match model_health.get(mid) {
                    None => {
                        reasons.push("no_eligible_model");
                        false
                    }
                    Some(status) if UNHEALTHY_MODEL_STATUSES.contains(&status.as_str()) => {
                        reasons.push("image_not_ready");
                        false
                    }
                    Some(_) => true,
                },
            };

            let evaluated_role = dispatched_role_for_status_type(&task_status, &issue_type);
            let toolset = super::queries::toolset_for_role(evaluated_role);
            let gate_verdict = if reasons.is_empty() {
                "stranded"
            } else {
                "blocked"
            };

            Some(serde_json::json!({
                "id":            row.get::<String, _>("id"),
                "short_id":      row.get::<String, _>("short_id"),
                "title":         row.get::<String, _>("title"),
                "status":        task_status,
                "owner":         row.get::<String, _>("owner"),
                "updated_at":    updated_at,
                "epic_short_id": row.get::<Option<String>, _>("epic_short_id"),
                "unclaimed_since": unclaimed_since,
                "unclaimed_since_confidence": confidence,
                "elapsed_minutes": elapsed,
                "severity":      severity,
                "threshold":     serde_json::json!({
                    "warning_minutes":  warning_threshold,
                    "error_minutes":    error_threshold,
                    "critical_minutes": critical_threshold,
                }),
                "dispatch_gate": serde_json::json!({
                    "evaluated_role":       evaluated_role,
                    "toolset":              toolset,
                    "model_requirement":    inflight_model_id,
                    "image_ready":          image_ready,
                    "breaker_open":         breaker_open,
                    "manually_paused":      manually_paused,
                    "rate_limited":         rate_limited,
                    "credential_available": credential_available,
                    "gate_verdict":         gate_verdict,
                    "reasons":              reasons,
                    // Retained for backward compatibility with the initial
                    // board_health contract.
                    "last_dispatched_role": row.get::<Option<String>, _>("last_dispatched_role"),
                    "cooldown_until":       cooldown_until,
                }),
            }))
        })
        .collect();

    serde_json::json!({
        "total":             findings.len(),
        "threshold_minutes": STRANDED_THRESHOLD_MINUTES,
        "findings":          findings,
    })
}

/// Closed-parent orphans: non-closed tasks whose parent scopes are terminal
/// (epic is closed or all linked proposals are terminal) and that have no
/// other open proposal parent.
///
/// Each finding carries enough evidence for a later repair snapshot: task
/// identity/status, terminal epic/proposal ids, other open parent ids, external
/// open-dependent evidence, and the recommended disposition derived from the
/// shared parent-disposition matrix. The query is read-only and does not mutate
/// any task or activity row.
pub(super) async fn closed_parent_open_children_section(pool: &sqlx::PgPool) -> serde_json::Value {
    // Runtime query: the section is additive and must not add entries to the
    // offline sqlx cache. We aggregate evidence as JSONB so the row shape is
    // stable regardless of how many proposals/dependents a task has.
    let rows = sqlx::query(
        r#"SELECT
              t.id AS task_id,
              t.short_id,
              t.title,
              t.status,
              t.updated_at,
              t.epic_id,
              e.short_id AS epic_short_id,
              e.status AS epic_status,
              COALESCE(
                (SELECT jsonb_agg(p.id ORDER BY p.id)
                 FROM proposal_epics pe
                 JOIN proposals p ON p.id = pe.proposal_id
                 WHERE pe.epic_id = e.id
                   AND p.status IN ('archived', 'superseded', 'rejected', 'done')),
                '[]'::jsonb
              ) AS terminal_proposal_ids,
              COALESCE(
                (SELECT jsonb_agg(p.id ORDER BY p.id)
                 FROM proposal_epics pe
                 JOIN proposals p ON p.id = pe.proposal_id
                 WHERE pe.epic_id = e.id
                   AND p.status NOT IN ('archived', 'superseded', 'rejected', 'done')),
                '[]'::jsonb
              ) AS open_proposal_ids,
              COALESCE(
                (SELECT jsonb_agg(
                    jsonb_build_object(
                      'task_id', dep.id,
                      'short_id', dep.short_id,
                      'title', dep.title,
                      'status', dep.status
                    ) ORDER BY dep.created_at
                 )
                 FROM blockers b
                 JOIN tasks dep ON dep.id = b.task_id
                 WHERE b.blocking_task_id = t.id
                   AND dep.status <> 'closed'
                   AND (dep.epic_id IS NULL OR dep.epic_id <> t.epic_id)),
                '[]'::jsonb
              ) AS external_open_dependents
           FROM tasks t
           JOIN epics e ON t.epic_id = e.id
           WHERE t.status <> 'closed'
           ORDER BY t.created_at"#,
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let mut findings = Vec::with_capacity(rows.len());
    for row in rows {
        let epic_status: String = row.get("epic_status");
        let epic_id: Option<String> = row.get("epic_id");
        let terminal_proposal_ids: serde_json::Value = row.get("terminal_proposal_ids");
        let open_proposal_ids: serde_json::Value = row.get("open_proposal_ids");
        let external_open_dependents: serde_json::Value = row.get("external_open_dependents");

        let terminal_proposals = terminal_proposal_ids
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        let open_parents = open_proposal_ids
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        let external_blockers = external_open_dependents
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false);

        // A closed-parent orphan must have at least one terminal parent scope
        // and no open proposal parent. The epic itself is an open parent when
        // it is not closed; in that case we require terminal proposals to
        // make the *proposal* parent scope terminal.
        let has_terminal_parent = epic_status == "closed" || terminal_proposals;
        if !has_terminal_parent {
            continue;
        }

        let status: String = row.get("status");
        let (recommended_action, recommended_status, recommended_reason) = if open_parents {
            ("retain", "none", "other_open_parent")
        } else if external_blockers {
            ("retain", "none", "external_open_dependent")
        } else if super::parent_disposition::CLOSE_READY_STATUSES.contains(&status.as_str()) {
            ("close", "closed", "parent_closed")
        } else if super::parent_disposition::PARK_STATUSES.contains(&status.as_str()) {
            let reason = super::parent_disposition::historical_park_reason_for_status(&status);
            ("park", "needs_lead_intervention", reason)
        } else {
            ("retain", "none", "unmapped_status")
        };

        findings.push(serde_json::json!({
            "id": row.get::<String, _>("task_id"),
            "short_id": row.get::<String, _>("short_id"),
            "title": row.get::<String, _>("title"),
            "status": status,
            "updated_at": row.get::<String, _>("updated_at"),
            "epic_id": epic_id,
            "epic_short_id": row.get::<Option<String>, _>("epic_short_id"),
            "epic_status": epic_status,
            "terminal_epic_ids": if epic_status == "closed" {
                serde_json::json!([epic_id])
            } else {
                serde_json::json!([])
            },
            "terminal_proposal_ids": terminal_proposal_ids,
            "other_open_parent_ids": open_proposal_ids,
            "external_open_dependents": external_open_dependents,
            "recommended_action": recommended_action,
            "recommended_status": recommended_status,
            "recommended_reason": recommended_reason,
        }));
    }

    serde_json::json!({
        "total": findings.len(),
        "findings": findings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    async fn project(db: &Database) -> String {
        db.ensure_initialized().await.unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        )
        .bind(&id)
        .bind("cp-test")
        .bind("test")
        .bind("cp-test")
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    async fn epic(db: &Database, project_id: &str, status: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&id)
        .bind(project_id)
        .bind(format!("e{}", &id[..4]))
        .bind("Epic")
        .bind("")
        .bind("")
        .bind("")
        .bind("")
        .bind(status)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    async fn task(db: &Database, epic_id: &str, status: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        let project_id: (String,) = sqlx::query_as("SELECT project_id FROM epics WHERE id = $1")
            .bind(epic_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        sqlx::query(
            r#"INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                    issue_type, status, priority, owner, labels, acceptance_criteria, memory_refs)
               VALUES ($1, $2, $3, $4, $5, '', '', 'task', $6, 1, '', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)"#,
        )
        .bind(&id)
        .bind(project_id.0)
        .bind(format!("t{}", &id[..4]))
        .bind(epic_id)
        .bind("Task")
        .bind(status)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    async fn proposal(db: &Database, status: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            r#"INSERT INTO proposals (id, short_id, title, body, body_format, acceptance_criteria, status, latest_revision_seq)
               VALUES ($1, $2, $3, '', 'markdown', '[]'::jsonb, $4, 1)"#,
        )
        .bind(&id)
        .bind(format!("p{}", &id[..4]))
        .bind("Proposal")
        .bind(status)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    async fn link_epic(db: &Database, proposal_id: &str, epic_id: &str, project_id: &str) {
        sqlx::query(
            "INSERT INTO proposal_epics (proposal_id, epic_id, project_id) VALUES ($1, $2, $3)",
        )
        .bind(proposal_id)
        .bind(epic_id)
        .bind(project_id)
        .execute(db.pool())
        .await
        .unwrap();
    }

    async fn blocker(db: &Database, blocked_id: &str, blocking_id: &str) {
        sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES ($1, $2)")
            .bind(blocked_id)
            .bind(blocking_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn closed_parent_section_reports_ready_orphan_for_close() {
        let db = Database::open_in_memory().unwrap();
        let project = project(&db).await;
        let epic = epic(&db, &project, "closed").await;
        let child = task(&db, &epic, "open").await;

        let section = closed_parent_open_children_section(db.pool()).await;
        let findings = section.get("findings").unwrap().as_array().unwrap();
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.get("id").unwrap().as_str(), Some(child.as_str()));
        assert_eq!(finding.get("status").unwrap().as_str(), Some("open"));
        assert_eq!(
            finding.get("recommended_action").unwrap().as_str(),
            Some("close")
        );
        assert_eq!(
            finding.get("recommended_status").unwrap().as_str(),
            Some("closed")
        );
        assert_eq!(
            finding.get("recommended_reason").unwrap().as_str(),
            Some("parent_closed")
        );
    }

    #[tokio::test]
    async fn closed_parent_section_parks_in_flight_orphan_with_historical_reason() {
        let db = Database::open_in_memory().unwrap();
        let project = project(&db).await;
        let epic = epic(&db, &project, "closed").await;
        let child = task(&db, &epic, "in_progress").await;

        let section = closed_parent_open_children_section(db.pool()).await;
        let finding = &section.get("findings").unwrap().as_array().unwrap()[0];
        assert_eq!(finding.get("id").unwrap().as_str(), Some(child.as_str()));
        assert_eq!(
            finding.get("recommended_action").unwrap().as_str(),
            Some("park")
        );
        assert_eq!(
            finding.get("recommended_status").unwrap().as_str(),
            Some("needs_lead_intervention")
        );
        assert_eq!(
            finding.get("recommended_reason").unwrap().as_str(),
            Some("historical_parent_closed_in_flight")
        );
    }

    #[tokio::test]
    async fn closed_parent_section_parks_pr_active_orphan_with_pr_reason() {
        let db = Database::open_in_memory().unwrap();
        let project = project(&db).await;
        let epic = epic(&db, &project, "closed").await;
        let _child = task(&db, &epic, "pr_review").await;

        let section = closed_parent_open_children_section(db.pool()).await;
        let finding = &section.get("findings").unwrap().as_array().unwrap()[0];
        assert_eq!(
            finding.get("recommended_action").unwrap().as_str(),
            Some("park")
        );
        assert_eq!(
            finding.get("recommended_reason").unwrap().as_str(),
            Some("historical_parent_closed_pr_active")
        );
    }

    #[tokio::test]
    async fn closed_parent_section_excludes_open_epic() {
        let db = Database::open_in_memory().unwrap();
        let project = project(&db).await;
        let epic = epic(&db, &project, "open").await;
        task(&db, &epic, "open").await;

        let section = closed_parent_open_children_section(db.pool()).await;
        assert_eq!(section.get("total").unwrap().as_i64(), Some(0));
    }

    #[tokio::test]
    async fn closed_parent_section_retains_other_open_proposal_parent() {
        let db = Database::open_in_memory().unwrap();
        let project = project(&db).await;
        let epic = epic(&db, &project, "closed").await;
        let open = proposal(&db, "building").await;
        link_epic(&db, &open, &epic, &project).await;
        let child = task(&db, &epic, "open").await;

        let section = closed_parent_open_children_section(db.pool()).await;
        let findings = section.get("findings").unwrap().as_array().unwrap();
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.get("id").unwrap().as_str(), Some(child.as_str()));
        assert_eq!(
            finding.get("recommended_action").unwrap().as_str(),
            Some("retain")
        );
        assert_eq!(
            finding.get("recommended_reason").unwrap().as_str(),
            Some("other_open_parent")
        );
        let other_parents = finding
            .get("other_open_parent_ids")
            .unwrap()
            .as_array()
            .unwrap();
        assert!(
            other_parents
                .iter()
                .any(|p| p.as_str() == Some(open.as_str()))
        );
    }

    #[tokio::test]
    async fn closed_parent_section_retains_external_open_dependent() {
        let db = Database::open_in_memory().unwrap();
        let project = project(&db).await;
        let closed_epic = epic(&db, &project, "closed").await;
        let open_epic = epic(&db, &project, "open").await;
        let child = task(&db, &closed_epic, "open").await;
        let dependent = task(&db, &open_epic, "open").await;
        blocker(&db, &dependent, &child).await;

        let section = closed_parent_open_children_section(db.pool()).await;
        let findings = section.get("findings").unwrap().as_array().unwrap();
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.get("id").unwrap().as_str(), Some(child.as_str()));
        assert_eq!(
            finding.get("recommended_action").unwrap().as_str(),
            Some("retain")
        );
        assert_eq!(
            finding.get("recommended_reason").unwrap().as_str(),
            Some("external_open_dependent")
        );
        let dependents = finding
            .get("external_open_dependents")
            .unwrap()
            .as_array()
            .unwrap();
        assert!(
            dependents
                .iter()
                .any(|d| d.get("task_id").unwrap().as_str() == Some(dependent.as_str()))
        );
    }

    #[tokio::test]
    async fn closed_parent_section_is_read_only() {
        let db = Database::open_in_memory().unwrap();
        let project = project(&db).await;
        let epic = epic(&db, &project, "closed").await;
        let child = task(&db, &epic, "open").await;

        let before: (String,) = sqlx::query_as("SELECT status FROM tasks WHERE id = $1")
            .bind(&child)
            .fetch_one(db.pool())
            .await
            .unwrap();
        let before_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM activity_log")
            .fetch_one(db.pool())
            .await
            .unwrap();

        closed_parent_open_children_section(db.pool()).await;
        closed_parent_open_children_section(db.pool()).await;

        let after: (String,) = sqlx::query_as("SELECT status FROM tasks WHERE id = $1")
            .bind(&child)
            .fetch_one(db.pool())
            .await
            .unwrap();
        let after_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM activity_log")
            .fetch_one(db.pool())
            .await
            .unwrap();

        assert_eq!(before.0, after.0);
        assert_eq!(before_count.0, after_count.0);
    }

    #[tokio::test]
    async fn terminal_proposal_makes_open_epic_orphan_when_no_other_open_proposal() {
        let db = Database::open_in_memory().unwrap();
        let project = project(&db).await;
        let epic = epic(&db, &project, "open").await;
        let terminal = proposal(&db, "done").await;
        link_epic(&db, &terminal, &epic, &project).await;
        let child = task(&db, &epic, "open").await;

        let section = closed_parent_open_children_section(db.pool()).await;
        let findings = section.get("findings").unwrap().as_array().unwrap();
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.get("id").unwrap().as_str(), Some(child.as_str()));
        let terminal_proposals = finding
            .get("terminal_proposal_ids")
            .unwrap()
            .as_array()
            .unwrap();
        assert!(
            terminal_proposals
                .iter()
                .any(|p| p.as_str() == Some(terminal.as_str()))
        );
    }

    #[tokio::test]
    async fn terminal_proposal_does_not_orphan_when_open_proposal_also_linked() {
        let db = Database::open_in_memory().unwrap();
        let project = project(&db).await;
        let epic = epic(&db, &project, "open").await;
        let terminal = proposal(&db, "done").await;
        let open = proposal(&db, "building").await;
        link_epic(&db, &terminal, &epic, &project).await;
        link_epic(&db, &open, &epic, &project).await;
        task(&db, &epic, "open").await;

        let section = closed_parent_open_children_section(db.pool()).await;
        assert_eq!(section.get("total").unwrap().as_i64(), Some(0));
    }
}
