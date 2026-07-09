//! Coordinator wiring for emitting `tripwire.hold.released` events when a
//! human closes a `human-review-hold` remediation task.
//!
//! ## The gap this closes
//!
//! The active-hold interpreter ([`crate::tripwires::active_hold`]) clears a
//! tripwire hold only when it sees a `tripwire.hold.released` event covering
//! the held findings for the current head. Historically **no code emitted
//! that event**: closing a `human-review-hold` task stamped
//! `human_review_resolved_at` and unparked the source (via
//! `mark_human_review_resolved`) but never wrote the release. The merge-time
//! tripwire gate (`reconcile_tripwire_hold`) therefore stayed held for the
//! current head until a new commit superseded it — incident: hold task
//! `yynd` was closed by a human yet the gate never cleared.
//!
//! ## What this does
//!
//! When the coordinator observes a closed task carrying the
//! `human-review-hold` label, for every source task it was blocking it:
//!
//! 1. Loads the source and takes its current PR head SHA
//!    (`ci_github_head_sha`, the poller-maintained head).
//! 2. Recomputes the active-hold state over the source's activity entries
//!    ([`compute_active_hold_state`]).
//! 3. If a hold is active for the current head, emits one
//!    `tripwire.hold.released` event covering **all** currently-held finding
//!    keys, with `released_by = "human"` and rationale = the hold task's
//!    close reason (falling back to a fixed non-empty string).
//!
//! If no hold is active for the current head (already superseded / released),
//! emission is skipped — no spurious event. The producer is idempotent: the
//! release idempotency key is stable, and the interpreter tolerates duplicate
//! releases.

use super::*;

use crate::tripwires::{
    ActivityEntryRef, HUMAN_RELEASE_ACTOR, HUMAN_RELEASE_ROLE, TRIPWIRE_EVENT_HOLD_RELEASED,
    build_hold_released_payload, compute_active_hold_state,
};

impl CoordinatorActor {
    /// Emit `tripwire.hold.released` on each source task held by a just-closed
    /// hold-release task — a legacy `human-review-hold` remediation OR an
    /// autonomous `planner-park-escalation` (see
    /// [`crate::roles::releases_source_on_close`]). Best-effort and fail-open on the
    /// emission side: a failure to write a release must not wedge the close.
    pub(super) async fn emit_tripwire_release_on_hold_close(
        &self,
        hold_task: &djinn_core::models::Task,
    ) {
        if !crate::roles::releases_source_on_close(hold_task) {
            return;
        }

        let task_repo = self.task_repo();

        let blocked = match task_repo.list_blocked_by(&hold_task.id).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    hold_task_id = %hold_task.short_id,
                    error = %e,
                    "tripwire release: failed to list source tasks held by closed hold task"
                );
                return;
            }
        };
        if blocked.is_empty() {
            return;
        }

        // Rationale = the hold task's close reason; the pure builder applies a
        // non-empty fallback when it is absent/blank (payload validator rejects
        // an empty rationale).
        let rationale = hold_task.close_reason.clone().unwrap_or_default();
        let now = ::time::OffsetDateTime::now_utc()
            .format(&::time::format_description::well_known::Rfc3339)
            .unwrap_or_default();

        for blocked_ref in blocked {
            let source = match task_repo.get(&blocked_ref.task_id).await {
                Ok(Some(t)) => t,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(
                        hold_task_id = %hold_task.short_id,
                        source_task_id = %blocked_ref.task_id,
                        error = %e,
                        "tripwire release: failed to load source task"
                    );
                    continue;
                }
            };

            let entries = match task_repo.list_activity(&source.id).await {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        source_task_id = %source.short_id,
                        error = %e,
                        "tripwire release: failed to query source activity"
                    );
                    continue;
                }
            };
            let entry_refs: Vec<ActivityEntryRef> =
                entries.iter().map(ActivityEntryRef::from_entry).collect();

            // Release EVERY head that still carries an un-released
            // `tripwire.gate.held` — NOT just the task row's current GitHub
            // head. The merge-boundary gate keys "current head" on the head
            // pinned in the gate.held event (the CI-snapshot head at hold
            // time), which can lag the task row's `ci_github_head_sha` after a
            // later push. Releasing only the row head silently misses the hold
            // the gate actually sees (incident: task `7tmi` — gate head
            // `fd3d3670` vs row head `e4e0ccb8`).
            let mut held_heads = crate::tripwires::gate_held_head_shas(&entry_refs);
            if let Some(row_head) = source
                .ci_github_head_sha
                .as_deref()
                .filter(|s| !s.is_empty())
                && !held_heads.iter().any(|h| h == row_head)
            {
                held_heads.push(row_head.to_owned());
            }

            let pr_number = source
                .pr_url
                .as_deref()
                .and_then(|url| crate::pr_poller::parse_pr_url(url).map(|(_, _, n)| n));

            for head_sha in &held_heads {
                let state = compute_active_hold_state(&entry_refs, head_sha);
                if !state.held {
                    continue;
                }
                let payload = match build_hold_released_payload(
                    &state,
                    &source.id,
                    &source.project_id,
                    pr_number,
                    HUMAN_RELEASE_ACTOR,
                    HUMAN_RELEASE_ROLE,
                    &rationale,
                    &now,
                ) {
                    Ok(Some(p)) => p,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!(
                            source_task_id = %source.short_id,
                            error = %e,
                            "tripwire release: failed to build hold-released payload"
                        );
                        continue;
                    }
                };

                let payload_json = serde_json::to_string(&payload).unwrap_or_default();
                match task_repo
                    .log_activity(
                        Some(&source.id),
                        HUMAN_RELEASE_ACTOR,
                        HUMAN_RELEASE_ROLE,
                        TRIPWIRE_EVENT_HOLD_RELEASED,
                        &payload_json,
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::info!(
                            source_task_id = %source.short_id,
                            hold_task_id = %hold_task.short_id,
                            head_sha = %head_sha,
                            released_findings = payload.released_findings.len(),
                            "tripwire release: emitted tripwire.hold.released on human hold-task close"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            source_task_id = %source.short_id,
                            error = %e,
                            "tripwire release: failed to log tripwire.hold.released event"
                        );
                    }
                }
            }

            // Shed the `human-review-hold` label from the source now the hold
            // is resolved. Otherwise the dispatch guard keeps skipping the
            // labeled source AND the tamper reconciler re-applies the label on
            // its next pass (labels were only ever added, never removed).
            // Best-effort — a failure here does not wedge the close.
            if crate::roles::is_human_review_hold(&source) {
                let cleaned = crate::roles::remove_hold_label_from_existing(&source.labels);
                if let Err(e) = task_repo.update_labels(&source.id, &cleaned).await {
                    tracing::warn!(
                        source_task_id = %source.short_id,
                        error = %e,
                        "tripwire release: failed to strip human-review-hold label after release"
                    );
                }
            }
        }
    }
}
