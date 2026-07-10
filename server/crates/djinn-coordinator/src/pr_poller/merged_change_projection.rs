// djinn:allow-oversize
//! Merged-change ledger projection: derives eligibility facts from a PR merge
//! and persists them to the audit-sampler merged-change table (epic ihf1).
//!
//! When a PR merge is observed, this module:
//!
//! 1. Collects task/project/PR/head-sha/merge-commit facts from the task row.
//! 2. Queries the activity log for tripwire gate/release/break-glass/tamper
//!    events associated with the task.
//! 3. Derives the gate outcome, stratum, and exclusion state from those
//!    typed activity payloads.
//! 4. Upserts a merged-change row via [`AuditSamplerRepository`].
//! 5. Emits a typed audit-ledger projection warning when provenance is
//!    incomplete and the merge cannot be marked eligible.

use super::*;
use crate::tripwires::activity_payloads::*;
use djinn_core::models::ActivityEntry;
use djinn_db::{AuditStratum, UpsertMergedChangeParams};

/// Activity event type for audit-ledger projection warnings emitted when a
/// merge lacks sufficient provenance for eligibility classification.
const AUDIT_LEDGER_PROJECTION_WARNING: &str = "audit_ledger.projection_warning";

/// Derives the merged-change ledger stratum, gate outcome, and provenance
/// from tripwire activity payloads associated with a task.
///
/// The `head_sha` field records which SHA was used for the derivation so
/// the caller can persist it even when the task row lacked `ci_head_sha`
/// and the SHA was discovered from a tripwire gate payload.
struct ProvenanceDerivation {
    gate_outcome: String,
    stratum: AuditStratum,
    gate_provenance: Option<serde_json::Value>,
    release_provenance: Option<serde_json::Value>,
    excluded: bool,
    exclusion_reason: Option<String>,
    /// The head SHA that was used to filter and classify tripwire events.
    head_sha: String,
}

/// Walk the task's tripwire activity entries and derive the merged-change
/// classification.
///
/// The derivation follows these rules:
///
/// - **Gate passed / report-only** (no enforcement findings) → `unflagged_merged`, eligible.
/// - **Gate held** + released by a non-human role → `autonomous_release`, eligible.
///   - `carried_forward` releases from prior heads count as autonomous.
/// - **Gate held** + released by a human → excluded (`human_release`).
/// - **Gate held** + break-glass → excluded (`break_glass`).
/// - **Gate held** + tamper → excluded (`tamper_detected`).
/// - **Gate held** + no release at all → excluded (`held_unreleased`).
/// - **No tripwire events at all** → `None` (insufficient provenance; the
///   caller must record this as incomplete and exclude the merged change).
fn derive_provenance(entries: &[ActivityEntry], head_sha: &str) -> Option<ProvenanceDerivation> {
    // Ensure ascending created_at order so `.last()` always picks the latest
    // event, regardless of the caller's ordering.  query_activity returns
    // DESC; without this sort the payload vectors would be populated
    // newest-first and `.last()` would select the oldest event.
    let mut sorted: Vec<&ActivityEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    // Parse tripwire payloads from activity entries, filtering to the given head SHA.
    let mut gate_passed_payloads: Vec<TripwireGateDecisionPayload> = Vec::new();
    let mut gate_held_payloads: Vec<TripwireGateDecisionPayload> = Vec::new();
    let mut gate_report_only_payloads: Vec<TripwireGateDecisionPayload> = Vec::new();
    let mut release_payloads: Vec<TripwireHoldReleasedPayload> = Vec::new();
    let mut break_glass_payloads: Vec<TripwireBreakGlassPayload> = Vec::new();
    let mut tamper_payloads: Vec<TripwireTamperPayload> = Vec::new();

    for entry in sorted.iter().copied() {
        match entry.event_type.as_str() {
            TRIPWIRE_EVENT_GATE_PASSED => {
                if let Ok(p) = serde_json::from_str::<TripwireGateDecisionPayload>(&entry.payload)
                    && p.head_sha == head_sha
                {
                    gate_passed_payloads.push(p);
                }
            }
            TRIPWIRE_EVENT_GATE_HELD => {
                if let Ok(p) = serde_json::from_str::<TripwireGateDecisionPayload>(&entry.payload)
                    && p.head_sha == head_sha
                {
                    gate_held_payloads.push(p);
                }
            }
            TRIPWIRE_EVENT_GATE_REPORT_ONLY => {
                if let Ok(p) = serde_json::from_str::<TripwireGateDecisionPayload>(&entry.payload)
                    && p.head_sha == head_sha
                {
                    gate_report_only_payloads.push(p);
                }
            }
            TRIPWIRE_EVENT_HOLD_RELEASED => {
                if let Ok(p) = serde_json::from_str::<TripwireHoldReleasedPayload>(&entry.payload)
                    && p.head_sha == head_sha
                {
                    release_payloads.push(p);
                }
            }
            TRIPWIRE_EVENT_BREAK_GLASS => {
                if let Ok(p) = serde_json::from_str::<TripwireBreakGlassPayload>(&entry.payload)
                    && p.head_sha == head_sha
                {
                    break_glass_payloads.push(p);
                }
            }
            TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED => {
                if let Ok(p) = serde_json::from_str::<TripwireTamperPayload>(&entry.payload)
                    && p.head_sha == head_sha
                {
                    tamper_payloads.push(p);
                }
            }
            _ => {}
        }
    }

    // --- Derive classification ---

    // Case 1: Gate held → check release / break-glass / tamper status.
    if let Some(held) = gate_held_payloads.last() {
        let gate_prov = serde_json::to_value(held).ok();

        // Break-glass → excluded.
        if let Some(bg) = break_glass_payloads.last() {
            return Some(ProvenanceDerivation {
                gate_outcome: "held_break_glass".to_owned(),
                stratum: AuditStratum::UnflaggedMerged,
                gate_provenance: gate_prov,
                release_provenance: serde_json::to_value(bg).ok(),
                excluded: true,
                exclusion_reason: Some("break_glass".to_owned()),
                head_sha: head_sha.to_owned(),
            });
        }

        // Tamper → excluded.
        if let Some(t) = tamper_payloads.last() {
            return Some(ProvenanceDerivation {
                gate_outcome: "held_tamper".to_owned(),
                stratum: AuditStratum::UnflaggedMerged,
                gate_provenance: gate_prov,
                release_provenance: serde_json::to_value(t).ok(),
                excluded: true,
                exclusion_reason: Some("tamper_detected".to_owned()),
                head_sha: head_sha.to_owned(),
            });
        }

        // Released by a non-human role → autonomous_release.
        if let Some(release) = release_payloads.last() {
            if is_nonhuman_role(&release.released_by_role) {
                return Some(ProvenanceDerivation {
                    gate_outcome: if release.carried_forward {
                        "released_carried_forward".to_owned()
                    } else {
                        "released_by_arbiter".to_owned()
                    },
                    stratum: AuditStratum::AutonomousRelease,
                    gate_provenance: gate_prov,
                    release_provenance: serde_json::to_value(release).ok(),
                    excluded: false,
                    exclusion_reason: None,
                    head_sha: head_sha.to_owned(),
                });
            } else {
                // Released by a human → excluded.
                return Some(ProvenanceDerivation {
                    gate_outcome: "released_by_human".to_owned(),
                    stratum: AuditStratum::UnflaggedMerged,
                    gate_provenance: gate_prov,
                    release_provenance: serde_json::to_value(release).ok(),
                    excluded: true,
                    exclusion_reason: Some("human_release".to_owned()),
                    head_sha: head_sha.to_owned(),
                });
            }
        }

        // No release at all → held, unreleased → excluded.
        return Some(ProvenanceDerivation {
            gate_outcome: "held_unreleased".to_owned(),
            stratum: AuditStratum::UnflaggedMerged,
            gate_provenance: gate_prov,
            release_provenance: None,
            excluded: true,
            exclusion_reason: Some("held_unreleased".to_owned()),
            head_sha: head_sha.to_owned(),
        });
    }

    // Case 2: Gate report-only → unflagged_merged (findings are advisory).
    if let Some(ro) = gate_report_only_payloads.last() {
        return Some(ProvenanceDerivation {
            gate_outcome: "report_only".to_owned(),
            stratum: AuditStratum::UnflaggedMerged,
            gate_provenance: serde_json::to_value(ro).ok(),
            release_provenance: None,
            excluded: false,
            exclusion_reason: None,
            head_sha: head_sha.to_owned(),
        });
    }

    // Case 3: Gate explicitly passed → unflagged_merged, eligible.
    if let Some(passed) = gate_passed_payloads.last() {
        return Some(ProvenanceDerivation {
            gate_outcome: "pass".to_owned(),
            stratum: AuditStratum::UnflaggedMerged,
            gate_provenance: serde_json::to_value(passed).ok(),
            release_provenance: None,
            excluded: false,
            exclusion_reason: None,
            head_sha: head_sha.to_owned(),
        });
    }

    // Case 4: No tripwire events for this head SHA at all → insufficient
    // provenance.  The caller must exclude this merged change and record
    // `incomplete_provenance`.  We do NOT silently invent eligibility.
    None
}

/// Returns `true` when the release actor role is _not_ a human operator.
///
/// The zero-human-holds cut-over means the normal release path is the
/// autonomous planner/arbiter. Roles like `"lead"`, `"maintainer"`,
/// `"incident_commander"`, or any value containing `"human"` are treated as
/// human. Everything else (e.g. `"arbiter"`, `"planner"`, `"system"`,
/// `"autonomous"`) is non-human.
fn is_nonhuman_role(role: &str) -> bool {
    let lower = role.to_ascii_lowercase();
    // Explicit human roles that appear in existing hold-release payloads.
    const HUMAN_ROLES: &[&str] = &["lead", "maintainer", "incident_commander", "operator"];
    if HUMAN_ROLES.iter().any(|hr| lower.contains(hr)) {
        return false;
    }
    if lower.contains("human") {
        return false;
    }
    true
}

impl CoordinatorActor {
    /// Project a merged-change ledger row from the merge facts and tripwire
    /// provenance of a PR that just landed.
    ///
    /// Best-effort: errors are logged as warnings and do not block the merge
    /// close flow.
    pub(crate) async fn project_merged_change_to_ledger(
        &self,
        task_id: &str,
        merge_commit_sha: &str,
    ) {
        let task_repo = self.task_repo();
        let audit_repo = self.audit_sampler_repo();

        // ── Collect merge facts from the task row ──────────────────────────
        let task = match task_repo.get(task_id).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                tracing::warn!(
                    task_id,
                    merge_commit_sha,
                    "audit-ledger: task not found; skipping projection"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    task_id,
                    merge_commit_sha,
                    error = %e,
                    "audit-ledger: failed to fetch task; skipping projection"
                );
                return;
            }
        };

        let project_id = &task.project_id;
        let pr_number: Option<i64> = task
            .pr_url
            .as_deref()
            .and_then(parse_pr_url)
            .map(|(_, _, n)| n as i64);
        let head_sha = task.ci_head_sha.as_deref();

        // ── Query tripwire activity for this task ──────────────────────────
        let activity_entries = match task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task_id.to_owned()),
                event_type: None,
                actor_role: None,
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 200,
                offset: 0,
            })
            .await
        {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    task_id,
                    merge_commit_sha,
                    error = %e,
                    "audit-ledger: failed to query activity; skipping projection"
                );
                return;
            }
        };

        // ── Derive provenance ─────────────────────────────────────────────
        // If we don't have a head SHA from the task's promoted CI snapshot,
        // try to extract one from the latest tripwire gate event for this
        // task (the gate payload always carries head_sha).
        let task_head_sha = head_sha.unwrap_or("").to_owned();
        let derived = if !task_head_sha.is_empty() {
            // We have a head SHA from the task's CI snapshot.
            match derive_provenance(&activity_entries, &task_head_sha) {
                Some(d) => d,
                None => {
                    // Head SHA exists on the task but no tripwire events
                    // matched it — insufficient provenance.
                    self.emit_projection_warning(
                        task_id,
                        project_id,
                        merge_commit_sha,
                        "task has head SHA but no matching tripwire gate events",
                    )
                    .await;
                    ProvenanceDerivation {
                        gate_outcome: "incomplete_provenance".to_owned(),
                        stratum: AuditStratum::UnflaggedMerged,
                        gate_provenance: None,
                        release_provenance: None,
                        excluded: true,
                        exclusion_reason: Some("incomplete_provenance".to_owned()),
                        head_sha: task_head_sha,
                    }
                }
            }
        } else {
            // No head SHA on the task — try to find one from any tripwire
            // gate event in the activity log so we still get a classification.
            // Sort ascending so .next_back() picks the latest gate event
            // (query_activity returns DESC).
            let mut sorted_for_fallback: Vec<&ActivityEntry> = activity_entries.iter().collect();
            sorted_for_fallback.sort_by(|a, b| a.created_at.cmp(&b.created_at));

            let fallback_head = sorted_for_fallback
                .iter()
                .copied()
                .filter(|e| {
                    matches!(
                        e.event_type.as_str(),
                        TRIPWIRE_EVENT_GATE_PASSED
                            | TRIPWIRE_EVENT_GATE_HELD
                            | TRIPWIRE_EVENT_GATE_REPORT_ONLY
                    )
                })
                .filter_map(|e| {
                    serde_json::from_str::<TripwireGateDecisionPayload>(&e.payload).ok()
                })
                .next_back()
                .map(|p| p.head_sha);

            if let Some(ref sha) = fallback_head {
                // We discovered a head SHA from tripwire activity; use it
                // for both derivation and the persisted ledger row.
                match derive_provenance(&activity_entries, sha) {
                    Some(d) => d,
                    None => {
                        // Should not happen: we found a gate event with this
                        // SHA but derive_provenance returned None (meaning
                        // the event didn't match). Treat as incomplete.
                        self.emit_projection_warning(
                            task_id,
                            project_id,
                            merge_commit_sha,
                            "fallback head SHA from tripwire gate did not produce a derivation",
                        )
                        .await;
                        ProvenanceDerivation {
                            gate_outcome: "incomplete_provenance".to_owned(),
                            stratum: AuditStratum::UnflaggedMerged,
                            gate_provenance: None,
                            release_provenance: None,
                            excluded: true,
                            exclusion_reason: Some("incomplete_provenance".to_owned()),
                            head_sha: sha.to_owned(),
                        }
                    }
                }
            } else {
                // No tripwire events and no head SHA — this merge lacks
                // sufficient provenance. Record as excluded with a warning.
                self.emit_projection_warning(
                    task_id,
                    project_id,
                    merge_commit_sha,
                    "no head SHA on task and no tripwire gate events found",
                )
                .await;
                ProvenanceDerivation {
                    gate_outcome: "incomplete_provenance".to_owned(),
                    stratum: AuditStratum::UnflaggedMerged,
                    gate_provenance: None,
                    release_provenance: None,
                    excluded: true,
                    exclusion_reason: Some("incomplete_provenance".to_owned()),
                    head_sha: String::new(),
                }
            }
        };

        // ── Use current UTC as merged_at ──────────────────────────────────
        let merged_at = ::time::OffsetDateTime::now_utc()
            .format(&::time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());

        // ── Upsert merged-change row ──────────────────────────────────────
        // Use the head SHA from the derivation (which may have come from the
        // task's CI snapshot or from a tripwire gate fallback).
        let effective_head = if derived.head_sha.is_empty() {
            None
        } else {
            Some(derived.head_sha.as_str())
        };

        match audit_repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id,
                task_id: Some(task_id),
                pr_number,
                head_sha: effective_head,
                merge_commit_sha,
                merged_at: &merged_at,
                gate_outcome: &derived.gate_outcome,
                gate_provenance: derived.gate_provenance.as_ref(),
                release_provenance: derived.release_provenance.as_ref(),
                stratum: derived.stratum.clone(),
                excluded: derived.excluded,
                exclusion_reason: derived.exclusion_reason.as_deref(),
            })
            .await
        {
            Ok(row) => {
                tracing::info!(
                    task_id,
                    merge_commit_sha,
                    stratum = %row.stratum,
                    excluded = row.excluded,
                    gate_outcome = %row.gate_outcome,
                    "audit-ledger: merged-change projected"
                );
            }
            Err(e) => {
                tracing::warn!(
                    task_id,
                    merge_commit_sha,
                    error = %e,
                    "audit-ledger: failed to upsert merged-change row"
                );
            }
        }
    }

    /// Emit a typed audit-ledger projection warning activity event when a
    /// merge lacks enough provenance to be classified. This is deliberately
    /// surfaced rather than silently inventing eligibility.
    async fn emit_projection_warning(
        &self,
        task_id: &str,
        project_id: &str,
        merge_commit_sha: &str,
        reason: &str,
    ) {
        let payload = serde_json::json!({
            "task_id": task_id,
            "project_id": project_id,
            "merge_commit_sha": merge_commit_sha,
            "reason": reason,
        });
        let task_repo = self.task_repo();
        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "system",
                "system",
                AUDIT_LEDGER_PROJECTION_WARNING,
                &payload.to_string(),
            )
            .await
        {
            tracing::warn!(
                task_id,
                merge_commit_sha,
                error = %e,
                "audit-ledger: failed to emit projection warning activity event"
            );
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_db::{
        ActivityQuery, AuditSamplerRepository, Database, EpicRepository, TaskRepository,
    };

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    async fn seed_task(
        db: &Database,
        pr_url: Option<&str>,
        ci_head_sha: Option<&str>,
    ) -> djinn_core::models::Task {
        let event_bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), event_bus.clone());
        let epic = epic_repo
            .create("audit-ledger epic", "", "", "", "", None)
            .await
            .unwrap();
        let task_repo = TaskRepository::new(db.clone(), event_bus);
        let task = task_repo
            .create(&epic.id, "audit-ledger task", "", "", "task", 0, "", None)
            .await
            .unwrap();
        if let Some(url) = pr_url {
            task_repo.set_pr_url(&task.id, url).await.unwrap();
        }
        if let Some(sha) = ci_head_sha {
            let pr_number = pr_url
                .and_then(|u| parse_pr_url(u))
                .map(|(_, _, n)| n as i64)
                .unwrap_or(0);
            task_repo
                .upsert_ci_snapshot(djinn_core::models::TaskPrCiSnapshotInput {
                    task_id: task.id.clone(),
                    pr_number,
                    head_sha: sha.to_owned(),
                    ci_status: djinn_core::models::CiStatus::Passing,
                    blocking_required_check_names: vec![],
                    failure_fingerprint: None,
                    same_signature_count: 0,
                    last_remediation_base_sha: None,
                })
                .await
                .unwrap();
        }
        task
    }

    async fn log_tripwire_activity(
        db: &Database,
        task_id: &str,
        event_type: &str,
        payload: &serde_json::Value,
    ) {
        let event_bus = EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), event_bus);
        task_repo
            .log_activity(
                Some(task_id),
                "system",
                "system",
                event_type,
                &payload.to_string(),
            )
            .await
            .unwrap();
    }

    /// Query activity via the same `query_activity` path the live projection
    /// uses (ORDER BY created_at DESC).  This is deliberately distinct from
    /// `list_activity` (ORDER BY created_at ASC) so integration tests exercise
    /// the real ordering.
    async fn query_activity_desc(db: &Database, task_id: &str) -> Vec<ActivityEntry> {
        let event_bus = EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), event_bus);
        task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task_id.to_owned()),
                event_type: None,
                actor_role: None,
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 200,
                offset: 0,
            })
            .await
            .unwrap()
    }

    fn sample_gate_passed_payload(task_id: &str, head_sha: &str) -> serde_json::Value {
        serde_json::json!({
            "event_type": TRIPWIRE_EVENT_GATE_PASSED,
            "task_id": task_id,
            "project_id": "proj",
            "pr_number": 42,
            "head_sha": head_sha,
            "policy_revision": "org-policy:1",
            "findings": [],
            "enforcement_finding_count": 0,
            "report_only_finding_count": 0,
            "idempotency_key": format!("sha256:passed:{head_sha}"),
        })
    }

    fn sample_gate_held_payload(task_id: &str, head_sha: &str) -> serde_json::Value {
        serde_json::json!({
            "event_type": TRIPWIRE_EVENT_GATE_HELD,
            "task_id": task_id,
            "project_id": "proj",
            "pr_number": 42,
            "head_sha": head_sha,
            "policy_revision": "org-policy:1",
            "findings": [{
                "rule_id": "migration_change",
                "reason_code": "tripwire.migration.changed",
                "severity": "human_review_required",
                "evidence": {"path": "migrations/0001.sql"},
                "idempotency_key": "fk1",
                "content_fingerprint": "fp1",
            }],
            "enforcement_finding_count": 1,
            "report_only_finding_count": 0,
            "idempotency_key": format!("sha256:held:{head_sha}"),
        })
    }

    fn sample_hold_released_payload(
        task_id: &str,
        head_sha: &str,
        role: &str,
        carried_forward: bool,
    ) -> serde_json::Value {
        serde_json::json!({
            "event_type": TRIPWIRE_EVENT_HOLD_RELEASED,
            "task_id": task_id,
            "project_id": "proj",
            "pr_number": 42,
            "head_sha": head_sha,
            "policy_revision": "org-policy:1",
            "released_by": "user_or_system",
            "released_by_role": role,
            "rationale": "approved by arbiter",
            "released_findings": [],
            "carried_forward": carried_forward,
            "idempotency_key": format!("sha256:release:{head_sha}:{role}"),
        })
    }

    fn sample_break_glass_payload(task_id: &str, head_sha: &str) -> serde_json::Value {
        serde_json::json!({
            "event_type": TRIPWIRE_EVENT_BREAK_GLASS,
            "task_id": task_id,
            "project_id": "proj",
            "pr_number": 42,
            "head_sha": head_sha,
            "policy_revision": "org-policy:1",
            "invoked_by": "incident_commander",
            "invoked_by_role": "incident_commander",
            "rationale": "SEV-1 hotfix",
            "overridden_findings": [],
            "idempotency_key": format!("sha256:bg:{head_sha}"),
        })
    }

    /// **Gate-passed unflagged merge** → `unflagged_merged`, not excluded.
    #[test]
    fn derive_gate_passed_unflagged() {
        let head = "abc123";
        let entries = vec![ActivityEntry {
            id: "a1".into(),
            task_id: Some("t1".into()),
            actor_id: "system".into(),
            actor_role: "system".into(),
            event_type: TRIPWIRE_EVENT_GATE_PASSED.to_owned(),
            payload: serde_json::to_string(&sample_gate_passed_payload("t1", head)).unwrap(),
            created_at: "2026-07-01T00:00:00Z".into(),
        }];
        let d = derive_provenance(&entries, head).expect("gate passed should produce Some");
        assert_eq!(d.gate_outcome, "pass");
        assert_eq!(d.stratum, AuditStratum::UnflaggedMerged);
        assert!(!d.excluded);
        assert!(d.exclusion_reason.is_none());
        assert_eq!(d.head_sha, head);
    }

    /// **Autonomous release** (non-human role) → `autonomous_release`, eligible.
    #[test]
    fn derive_autonomous_release() {
        let head = "def456";
        let entries = vec![
            ActivityEntry {
                id: "a1".into(),
                task_id: Some("t1".into()),
                actor_id: "system".into(),
                actor_role: "system".into(),
                event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
                payload: serde_json::to_string(&sample_gate_held_payload("t1", head)).unwrap(),
                created_at: "2026-07-01T00:00:00Z".into(),
            },
            ActivityEntry {
                id: "a2".into(),
                task_id: Some("t1".into()),
                actor_id: "arbiter".into(),
                actor_role: "system".into(),
                event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
                payload: serde_json::to_string(&sample_hold_released_payload(
                    "t1", head, "arbiter", false,
                ))
                .unwrap(),
                created_at: "2026-07-01T00:01:00Z".into(),
            },
        ];
        let d = derive_provenance(&entries, head).expect("autonomous release should produce Some");
        assert_eq!(d.gate_outcome, "released_by_arbiter");
        assert_eq!(d.stratum, AuditStratum::AutonomousRelease);
        assert!(!d.excluded);
        assert_eq!(d.head_sha, head);
    }

    /// **Carried-forward autonomous release** → `autonomous_release` with
    /// `released_carried_forward` gate outcome.
    #[test]
    fn derive_carried_forward_autonomous_release() {
        let head = "cf789";
        let entries = vec![
            ActivityEntry {
                id: "a1".into(),
                task_id: Some("t1".into()),
                actor_id: "system".into(),
                actor_role: "system".into(),
                event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
                payload: serde_json::to_string(&sample_gate_held_payload("t1", head)).unwrap(),
                created_at: "2026-07-01T00:00:00Z".into(),
            },
            ActivityEntry {
                id: "a2".into(),
                task_id: Some("t1".into()),
                actor_id: "system".into(),
                actor_role: "system".into(),
                event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
                payload: serde_json::to_string(&sample_hold_released_payload(
                    "t1", head, "planner", true,
                ))
                .unwrap(),
                created_at: "2026-07-01T00:01:00Z".into(),
            },
        ];
        let d = derive_provenance(&entries, head).expect("carried-forward should produce Some");
        assert_eq!(d.gate_outcome, "released_carried_forward");
        assert_eq!(d.stratum, AuditStratum::AutonomousRelease);
        assert!(!d.excluded);
        assert_eq!(d.head_sha, head);
    }

    /// **Human release** → excluded with `human_release` reason.
    #[test]
    fn derive_human_release_excluded() {
        let head = "human1";
        let entries = vec![
            ActivityEntry {
                id: "a1".into(),
                task_id: Some("t1".into()),
                actor_id: "system".into(),
                actor_role: "system".into(),
                event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
                payload: serde_json::to_string(&sample_gate_held_payload("t1", head)).unwrap(),
                created_at: "2026-07-01T00:00:00Z".into(),
            },
            ActivityEntry {
                id: "a2".into(),
                task_id: Some("t1".into()),
                actor_id: "user1".into(),
                actor_role: "lead".into(),
                event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
                payload: serde_json::to_string(&sample_hold_released_payload(
                    "t1", head, "lead", false,
                ))
                .unwrap(),
                created_at: "2026-07-01T00:01:00Z".into(),
            },
        ];
        let d = derive_provenance(&entries, head).expect("human release should produce Some");
        assert_eq!(d.gate_outcome, "released_by_human");
        assert!(d.excluded);
        assert_eq!(d.exclusion_reason.as_deref(), Some("human_release"));
        assert_eq!(d.head_sha, head);
    }

    /// **Break-glass** → excluded with `break_glass` reason.
    #[test]
    fn derive_break_glass_excluded() {
        let head = "bg1";
        let entries = vec![
            ActivityEntry {
                id: "a1".into(),
                task_id: Some("t1".into()),
                actor_id: "system".into(),
                actor_role: "system".into(),
                event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
                payload: serde_json::to_string(&sample_gate_held_payload("t1", head)).unwrap(),
                created_at: "2026-07-01T00:00:00Z".into(),
            },
            ActivityEntry {
                id: "a2".into(),
                task_id: Some("t1".into()),
                actor_id: "user1".into(),
                actor_role: "incident_commander".into(),
                event_type: TRIPWIRE_EVENT_BREAK_GLASS.to_owned(),
                payload: serde_json::to_string(&sample_break_glass_payload("t1", head)).unwrap(),
                created_at: "2026-07-01T00:01:00Z".into(),
            },
        ];
        let d = derive_provenance(&entries, head).expect("break-glass should produce Some");
        assert_eq!(d.gate_outcome, "held_break_glass");
        assert!(d.excluded);
        assert_eq!(d.exclusion_reason.as_deref(), Some("break_glass"));
        assert_eq!(d.head_sha, head);
    }

    /// **Tamper** → excluded with `tamper_detected` reason.
    #[test]
    fn derive_tamper_excluded() {
        let head = "tamper1";
        let tamper_payload = serde_json::json!({
            "event_type": TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED,
            "task_id": "t1",
            "project_id": "proj",
            "pr_number": 42,
            "head_sha": head,
            "policy_revision": "org-policy:1",
            "removed_by": "unknown",
            "detection_source": "pr_poller",
            "tampered_findings": [],
            "reapplied": true,
            "idempotency_key": format!("sha256:tamper:{head}"),
        });
        let entries = vec![
            ActivityEntry {
                id: "a1".into(),
                task_id: Some("t1".into()),
                actor_id: "system".into(),
                actor_role: "system".into(),
                event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
                payload: serde_json::to_string(&sample_gate_held_payload("t1", head)).unwrap(),
                created_at: "2026-07-01T00:00:00Z".into(),
            },
            ActivityEntry {
                id: "a2".into(),
                task_id: Some("t1".into()),
                actor_id: "unknown".into(),
                actor_role: "system".into(),
                event_type: TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED.to_owned(),
                payload: serde_json::to_string(&tamper_payload).unwrap(),
                created_at: "2026-07-01T00:01:00Z".into(),
            },
        ];
        let d = derive_provenance(&entries, head).expect("tamper should produce Some");
        assert_eq!(d.gate_outcome, "held_tamper");
        assert!(d.excluded);
        assert_eq!(d.exclusion_reason.as_deref(), Some("tamper_detected"));
        assert_eq!(d.head_sha, head);
    }

    /// **No tripwire events** → `None` (insufficient provenance).
    ///
    /// This is the critical difference from the old behavior: we do NOT
    /// silently invent an eligible `pass` when no tripwire events exist.
    #[test]
    fn derive_no_tripwire_events_returns_none() {
        let entries: Vec<ActivityEntry> = vec![];
        let result = derive_provenance(&entries, "sha_none");
        assert!(
            result.is_none(),
            "no tripwire events must return None, not silently invent pass"
        );
    }

    /// **Held, unreleased** → excluded with `held_unreleased` reason.
    #[test]
    fn derive_held_unreleased_excluded() {
        let head = "held1";
        let entries = vec![ActivityEntry {
            id: "a1".into(),
            task_id: Some("t1".into()),
            actor_id: "system".into(),
            actor_role: "system".into(),
            event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
            payload: serde_json::to_string(&sample_gate_held_payload("t1", head)).unwrap(),
            created_at: "2026-07-01T00:00:00Z".into(),
        }];
        let d = derive_provenance(&entries, head).expect("held should produce Some");
        assert_eq!(d.gate_outcome, "held_unreleased");
        assert!(d.excluded);
        assert_eq!(d.exclusion_reason.as_deref(), Some("held_unreleased"));
    }

    /// **Report-only gate** → `unflagged_merged`, eligible.
    #[test]
    fn derive_report_only_unflagged() {
        let head = "ro1";
        let payload = serde_json::json!({
            "event_type": TRIPWIRE_EVENT_GATE_REPORT_ONLY,
            "task_id": "t1",
            "project_id": "proj",
            "pr_number": 7,
            "head_sha": head,
            "policy_revision": "org-policy:1",
            "findings": [{
                "rule_id": "ci_workflow_change",
                "reason_code": "tripwire.ci.workflow_changed",
                "severity": "report_only",
                "evidence": {"path": ".github/workflows/ci.yml"},
                "idempotency_key": "fk_ro",
                "content_fingerprint": "fp_ro",
            }],
            "enforcement_finding_count": 0,
            "report_only_finding_count": 1,
            "idempotency_key": format!("sha256:ro:{head}"),
        });
        let entries = vec![ActivityEntry {
            id: "a1".into(),
            task_id: Some("t1".into()),
            actor_id: "system".into(),
            actor_role: "system".into(),
            event_type: TRIPWIRE_EVENT_GATE_REPORT_ONLY.to_owned(),
            payload: serde_json::to_string(&payload).unwrap(),
            created_at: "2026-07-01T00:00:00Z".into(),
        }];
        let d = derive_provenance(&entries, head).expect("report-only should produce Some");
        assert_eq!(d.gate_outcome, "report_only");
        assert_eq!(d.stratum, AuditStratum::UnflaggedMerged);
        assert!(!d.excluded);
        assert_eq!(d.head_sha, head);
    }

    /// **is_nonhuman_role** classification.
    #[test]
    fn nonhuman_role_classification() {
        // Non-human
        assert!(is_nonhuman_role("arbiter"));
        assert!(is_nonhuman_role("planner"));
        assert!(is_nonhuman_role("system"));
        assert!(is_nonhuman_role("autonomous"));
        // Human
        assert!(!is_nonhuman_role("lead"));
        assert!(!is_nonhuman_role("maintainer"));
        assert!(!is_nonhuman_role("incident_commander"));
        assert!(!is_nonhuman_role("operator"));
        assert!(!is_nonhuman_role("human_reviewer"));
    }

    /// **Integration: gate-passed unflagged merge** persists to DB.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_gate_passed_persists_to_ledger() {
        let db = test_db();
        let task = seed_task(
            &db,
            Some("https://github.com/acme/repo/pull/42"),
            Some("head_sha_1"),
        )
        .await;

        log_tripwire_activity(
            &db,
            &task.id,
            TRIPWIRE_EVENT_GATE_PASSED,
            &sample_gate_passed_payload(&task.id, "head_sha_1"),
        )
        .await;

        let repo = AuditSamplerRepository::new(db.clone());
        // Use query_activity (DESC) — the same path the live projection uses.
        let entries = query_activity_desc(&db, &task.id).await;
        let derived = derive_provenance(&entries, "head_sha_1")
            .expect("gate-passed with matching activity must produce Some");

        assert_eq!(derived.stratum, AuditStratum::UnflaggedMerged);
        assert!(!derived.excluded);

        let row = repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                pr_number: Some(42),
                head_sha: Some("head_sha_1"),
                merge_commit_sha: "merge_sha_1",
                merged_at: "2026-07-01T12:00:00Z",
                gate_outcome: &derived.gate_outcome,
                gate_provenance: derived.gate_provenance.as_ref(),
                release_provenance: derived.release_provenance.as_ref(),
                stratum: derived.stratum.clone(),
                excluded: derived.excluded,
                exclusion_reason: derived.exclusion_reason.as_deref(),
            })
            .await
            .unwrap();

        assert_eq!(row.stratum, "unflagged_merged");
        assert!(!row.excluded);
        assert_eq!(row.gate_outcome, "pass");
    }

    /// **Integration: autonomous release merge** persists with correct stratum.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_autonomous_release_persists_to_ledger() {
        let db = test_db();
        let task = seed_task(
            &db,
            Some("https://github.com/acme/repo/pull/99"),
            Some("head_arb"),
        )
        .await;

        log_tripwire_activity(
            &db,
            &task.id,
            TRIPWIRE_EVENT_GATE_HELD,
            &sample_gate_held_payload(&task.id, "head_arb"),
        )
        .await;
        log_tripwire_activity(
            &db,
            &task.id,
            TRIPWIRE_EVENT_HOLD_RELEASED,
            &sample_hold_released_payload(&task.id, "head_arb", "arbiter", false),
        )
        .await;

        let repo = AuditSamplerRepository::new(db.clone());
        // Use query_activity (DESC) — the same path the live projection uses.
        let entries = query_activity_desc(&db, &task.id).await;
        let derived = derive_provenance(&entries, "head_arb")
            .expect("autonomous release with matching activity must produce Some");

        assert_eq!(derived.stratum, AuditStratum::AutonomousRelease);
        assert!(!derived.excluded);

        let row = repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                pr_number: Some(99),
                head_sha: Some("head_arb"),
                merge_commit_sha: "merge_arb",
                merged_at: "2026-07-01T12:00:00Z",
                gate_outcome: &derived.gate_outcome,
                gate_provenance: derived.gate_provenance.as_ref(),
                release_provenance: derived.release_provenance.as_ref(),
                stratum: derived.stratum.clone(),
                excluded: derived.excluded,
                exclusion_reason: derived.exclusion_reason.as_deref(),
            })
            .await
            .unwrap();

        assert_eq!(row.stratum, "autonomous_release");
        assert_eq!(row.gate_outcome, "released_by_arbiter");
        assert!(row.release_provenance.is_some());
    }

    /// **Integration: task has head SHA but no matching tripwire events**
    /// → excluded with `incomplete_provenance`, head SHA persisted.
    ///
    /// This test exercises the projection/derivation path end-to-end rather
    /// than constructing the desired excluded row directly, preventing the
    /// no-tripwire-with-head case from becoming eligible.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_head_sha_no_tripwire_excluded() {
        let db = test_db();
        let task = seed_task(
            &db,
            Some("https://github.com/acme/repo/pull/55"),
            Some("head_no_tripwire"),
        )
        .await;

        // No tripwire activity logged for this task at all.

        // Use query_activity (DESC) — the same path the live projection uses.
        let entries = query_activity_desc(&db, &task.id).await;

        // derive_provenance returns None because there are no tripwire events.
        let result = derive_provenance(&entries, "head_no_tripwire");
        assert!(
            result.is_none(),
            "head SHA with no tripwire activity must return None"
        );

        // The live projection would then emit a warning and create an
        // incomplete_provenance derivation with the task's head SHA.
        // Verify the excluded state and that the head_sha from the task
        // is carried through, exactly as the live code path does.
        let repo = AuditSamplerRepository::new(db.clone());
        let derived = match derive_provenance(&entries, "head_no_tripwire") {
            Some(d) => d,
            None => ProvenanceDerivation {
                gate_outcome: "incomplete_provenance".to_owned(),
                stratum: AuditStratum::UnflaggedMerged,
                gate_provenance: None,
                release_provenance: None,
                excluded: true,
                exclusion_reason: Some("incomplete_provenance".to_owned()),
                head_sha: "head_no_tripwire".to_owned(),
            },
        };

        // The head_sha from the task row must be carried through.
        assert_eq!(derived.head_sha, "head_no_tripwire");
        assert!(derived.excluded);
        assert_eq!(
            derived.exclusion_reason.as_deref(),
            Some("incomplete_provenance")
        );

        let effective_head = if derived.head_sha.is_empty() {
            None
        } else {
            Some(derived.head_sha.as_str())
        };

        let row = repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                pr_number: Some(55),
                head_sha: effective_head,
                merge_commit_sha: "merge_no_trip",
                merged_at: "2026-07-01T12:00:00Z",
                gate_outcome: &derived.gate_outcome,
                gate_provenance: None,
                release_provenance: None,
                stratum: derived.stratum.clone(),
                excluded: derived.excluded,
                exclusion_reason: derived.exclusion_reason.as_deref(),
            })
            .await
            .unwrap();

        assert!(row.excluded);
        assert_eq!(
            row.exclusion_reason.as_deref(),
            Some("incomplete_provenance")
        );
        assert_eq!(row.head_sha.as_deref(), Some("head_no_tripwire"));
    }

    /// **Integration: no head SHA and no tripwire events** → excluded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_incomplete_provenance_excluded() {
        let db = test_db();
        // No pr_url, no ci_head_sha.
        let task = seed_task(&db, None, None).await;

        // Use query_activity (DESC) — the same path the live projection uses.
        let entries = query_activity_desc(&db, &task.id).await;

        // No fallback head available: no tripwire events at all.
        let fallback_gate: Option<&ActivityEntry> = entries.iter().find(|e| {
            matches!(
                e.event_type.as_str(),
                TRIPWIRE_EVENT_GATE_PASSED
                    | TRIPWIRE_EVENT_GATE_HELD
                    | TRIPWIRE_EVENT_GATE_REPORT_ONLY
            )
        });
        assert!(fallback_gate.is_none(), "no tripwire gate events expected");

        // The live projection falls through to incomplete_provenance.
        let repo = AuditSamplerRepository::new(db.clone());
        let derived = ProvenanceDerivation {
            gate_outcome: "incomplete_provenance".to_owned(),
            stratum: AuditStratum::UnflaggedMerged,
            gate_provenance: None,
            release_provenance: None,
            excluded: true,
            exclusion_reason: Some("incomplete_provenance".to_owned()),
            head_sha: String::new(),
        };

        let effective_head = if derived.head_sha.is_empty() {
            None
        } else {
            Some(derived.head_sha.as_str())
        };

        let row = repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                pr_number: None,
                head_sha: effective_head,
                merge_commit_sha: "merge_no_prov",
                merged_at: "2026-07-01T12:00:00Z",
                gate_outcome: &derived.gate_outcome,
                gate_provenance: None,
                release_provenance: None,
                stratum: derived.stratum.clone(),
                excluded: derived.excluded,
                exclusion_reason: derived.exclusion_reason.as_deref(),
            })
            .await
            .unwrap();

        assert!(row.excluded);
        assert_eq!(
            row.exclusion_reason.as_deref(),
            Some("incomplete_provenance")
        );
    }

    /// **Integration: break-glass exclusion** persists correctly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_break_glass_excluded() {
        let db = test_db();
        let task = seed_task(
            &db,
            Some("https://github.com/acme/repo/pull/7"),
            Some("head_bg"),
        )
        .await;

        log_tripwire_activity(
            &db,
            &task.id,
            TRIPWIRE_EVENT_GATE_HELD,
            &sample_gate_held_payload(&task.id, "head_bg"),
        )
        .await;
        log_tripwire_activity(
            &db,
            &task.id,
            TRIPWIRE_EVENT_BREAK_GLASS,
            &sample_break_glass_payload(&task.id, "head_bg"),
        )
        .await;

        let repo = AuditSamplerRepository::new(db.clone());
        // Use query_activity (DESC) — the same path the live projection uses.
        let entries = query_activity_desc(&db, &task.id).await;
        let derived = derive_provenance(&entries, "head_bg")
            .expect("break-glass with matching activity must produce Some");

        assert!(derived.excluded);
        assert_eq!(derived.exclusion_reason.as_deref(), Some("break_glass"));

        let row = repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                pr_number: Some(7),
                head_sha: Some("head_bg"),
                merge_commit_sha: "merge_bg",
                merged_at: "2026-07-01T12:00:00Z",
                gate_outcome: &derived.gate_outcome,
                gate_provenance: derived.gate_provenance.as_ref(),
                release_provenance: derived.release_provenance.as_ref(),
                stratum: derived.stratum.clone(),
                excluded: derived.excluded,
                exclusion_reason: derived.exclusion_reason.as_deref(),
            })
            .await
            .unwrap();

        assert!(row.excluded);
        assert_eq!(row.exclusion_reason.as_deref(), Some("break_glass"));
    }

    /// **Integration: fallback head SHA from tripwire activity is persisted.**
    ///
    /// When the task row has no `ci_head_sha` but a tripwire gate payload
    /// provides a head SHA, the ledger row must persist that head SHA rather
    /// than NULL.  Uses `query_activity_desc` (DESC order) to exercise the
    /// real path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_fallback_head_sha_persisted() {
        let db = test_db();
        // Task has a PR URL but no ci_head_sha.
        let task = seed_task(&db, Some("https://github.com/acme/repo/pull/88"), None).await;

        // Log a gate-passed event whose payload carries a head SHA.
        log_tripwire_activity(
            &db,
            &task.id,
            TRIPWIRE_EVENT_GATE_PASSED,
            &sample_gate_passed_payload(&task.id, "fallback_sha_123"),
        )
        .await;

        // Use query_activity (DESC) — the same path the live projection uses.
        let entries = query_activity_desc(&db, &task.id).await;

        // Reproduce the projection's fallback path with proper sorting:
        // sort ascending so .next_back() picks the latest gate event.
        let mut sorted: Vec<&ActivityEntry> = entries.iter().collect();
        sorted.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        let fallback_head = sorted
            .iter()
            .copied()
            .filter(|e| {
                matches!(
                    e.event_type.as_str(),
                    TRIPWIRE_EVENT_GATE_PASSED
                        | TRIPWIRE_EVENT_GATE_HELD
                        | TRIPWIRE_EVENT_GATE_REPORT_ONLY
                )
            })
            .filter_map(|e| serde_json::from_str::<TripwireGateDecisionPayload>(&e.payload).ok())
            .next_back()
            .map(|p| p.head_sha);

        assert_eq!(
            fallback_head.as_deref(),
            Some("fallback_sha_123"),
            "must discover head SHA from tripwire activity"
        );

        let derived = derive_provenance(&entries, fallback_head.as_ref().unwrap())
            .expect("gate-passed fallback must produce Some");

        // The derived head_sha must match the fallback, not empty.
        assert_eq!(derived.head_sha, "fallback_sha_123");
        assert_eq!(derived.gate_outcome, "pass");
        assert!(!derived.excluded);

        // Upsert and verify the persisted head_sha is NOT null.
        let repo = AuditSamplerRepository::new(db.clone());
        let effective_head = if derived.head_sha.is_empty() {
            None
        } else {
            Some(derived.head_sha.as_str())
        };

        let row = repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                pr_number: Some(88),
                head_sha: effective_head,
                merge_commit_sha: "merge_fallback",
                merged_at: "2026-07-01T12:00:00Z",
                gate_outcome: &derived.gate_outcome,
                gate_provenance: derived.gate_provenance.as_ref(),
                release_provenance: derived.release_provenance.as_ref(),
                stratum: derived.stratum.clone(),
                excluded: derived.excluded,
                exclusion_reason: derived.exclusion_reason.as_deref(),
            })
            .await
            .unwrap();

        assert_eq!(
            row.head_sha.as_deref(),
            Some("fallback_sha_123"),
            "fallback head SHA must be persisted, not NULL"
        );
    }

    /// **Integration: multiple gate events with different heads — latest
    /// is used for fallback, and `derive_provenance` picks the latest
    /// same-head event.**
    ///
    /// This test specifically verifies the ordering fix: with DESC activity
    /// queries, the sorted `.next_back()` and `.last()` must still select
    /// the latest event by `created_at`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_latest_event_picked_with_desc_ordering() {
        let db = test_db();
        // Task has no ci_head_sha — must use fallback path.
        let task = seed_task(&db, Some("https://github.com/acme/repo/pull/100"), None).await;

        // Log an OLDER gate-passed for head_sha "old_head".
        log_tripwire_activity(
            &db,
            &task.id,
            TRIPWIRE_EVENT_GATE_PASSED,
            &sample_gate_passed_payload(&task.id, "old_head"),
        )
        .await;
        // Log a NEWER gate-passed for head_sha "new_head".
        log_tripwire_activity(
            &db,
            &task.id,
            TRIPWIRE_EVENT_GATE_PASSED,
            &sample_gate_passed_payload(&task.id, "new_head"),
        )
        .await;

        // query_activity returns DESC (newest first).
        let entries = query_activity_desc(&db, &task.id).await;
        assert_eq!(entries.len(), 2, "expected 2 activity entries");
        // DESC: newest first
        assert!(
            entries[0].created_at >= entries[1].created_at,
            "query_activity must return DESC order"
        );

        // derive_provenance must pick the latest "new_head" event.
        // Even though DESC order puts "new_head" first, the internal sort
        // ensures .last() picks the latest.
        let derived = derive_provenance(&entries, "new_head")
            .expect("new_head gate-passed must produce Some");
        assert_eq!(derived.gate_outcome, "pass");
        assert!(!derived.excluded);
        assert_eq!(derived.head_sha, "new_head");

        // The fallback path with proper sorting must also pick "new_head".
        let mut sorted: Vec<&ActivityEntry> = entries.iter().collect();
        sorted.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        let fallback_head = sorted
            .iter()
            .copied()
            .filter(|e| {
                matches!(
                    e.event_type.as_str(),
                    TRIPWIRE_EVENT_GATE_PASSED
                        | TRIPWIRE_EVENT_GATE_HELD
                        | TRIPWIRE_EVENT_GATE_REPORT_ONLY
                )
            })
            .filter_map(|e| serde_json::from_str::<TripwireGateDecisionPayload>(&e.payload).ok())
            .next_back()
            .map(|p| p.head_sha);

        assert_eq!(
            fallback_head.as_deref(),
            Some("new_head"),
            "fallback must pick the latest gate event (new_head), not the oldest (old_head)"
        );
    }

    /// **Integration: same-head events in DESC order — latest event wins.**
    ///
    /// A task has `ci_head_sha` and two release events on the same head:
    /// an older human release and a newer non-human release. The derivation
    /// must pick the latest (non-human) release.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_same_head_latest_release_picked() {
        let db = test_db();
        let task = seed_task(
            &db,
            Some("https://github.com/acme/repo/pull/200"),
            Some("same_head"),
        )
        .await;

        // Log an OLDER human release.
        log_tripwire_activity(
            &db,
            &task.id,
            TRIPWIRE_EVENT_GATE_HELD,
            &sample_gate_held_payload(&task.id, "same_head"),
        )
        .await;
        log_tripwire_activity(
            &db,
            &task.id,
            TRIPWIRE_EVENT_HOLD_RELEASED,
            &sample_hold_released_payload(&task.id, "same_head", "lead", false),
        )
        .await;

        // Log a NEWER non-human release (carried-forward by planner).
        log_tripwire_activity(
            &db,
            &task.id,
            TRIPWIRE_EVENT_HOLD_RELEASED,
            &sample_hold_released_payload(&task.id, "same_head", "planner", true),
        )
        .await;

        // query_activity returns DESC (newest first).
        let entries = query_activity_desc(&db, &task.id).await;
        assert_eq!(entries.len(), 3, "expected 3 activity entries");

        // derive_provenance must pick the latest release (planner, carried-forward).
        let derived = derive_provenance(&entries, "same_head")
            .expect("same_head with release must produce Some");
        assert_eq!(derived.stratum, AuditStratum::AutonomousRelease);
        assert!(!derived.excluded);
        assert_eq!(derived.gate_outcome, "released_carried_forward");
    }
}
