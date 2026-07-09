//! Tripwire hold creation + release carry-forward for the PR poller.
//!
//! Split out of `pr_watcher` so that module stays under the repo size guard
//! (`scripts/check-file-size.sh`). Both methods extend [`CoordinatorActor`]
//! and are invoked from the `GateOutcome::Held` arm of the draft-processing
//! loop.

use super::*;

impl CoordinatorActor {
    /// Release carry-forward for a `Held` gate on a new head.
    ///
    /// For each enforcement finding the gate produced on `head_sha`, if the
    /// same content (by head-independent
    /// [content fingerprint](crate::tripwires::build_finding_content_fingerprint))
    /// was already adjudicated + released on a PRIOR head of this PR and is
    /// byte-identical here — a rebase / merge of `main` that left the flagged
    /// file untouched — a fresh `tripwire.hold.released` (`carried_forward:
    /// true`) is emitted for the new head referencing the prior rationale.
    /// Genuinely changed content has a different fingerprint and is never
    /// carried forward (it re-holds).
    ///
    /// Returns `true` when EVERY enforcement finding was carried forward (the
    /// hold is cleared for this head — the caller proceeds to undraft), or
    /// `false` when at least one enforcement finding remains active (the caller
    /// must create the hold).
    pub(super) async fn carry_forward_tripwire_gate(
        &self,
        task: &djinn_core::models::Task,
        head_sha: &str,
        pull_number: u64,
        tripwire_result: &super::tripwire_gate::TripwireGateResult,
    ) -> bool {
        let now = ::time::OffsetDateTime::now_utc()
            .format(&::time::format_description::well_known::Rfc3339)
            .unwrap_or_default();

        let enforcement_summaries: Vec<crate::tripwires::TripwireFindingSummary> = tripwire_result
            .payload
            .findings
            .iter()
            .filter(|f| f.severity == crate::tripwires::TripwireSeverity::HumanReviewRequired)
            .cloned()
            .collect();
        if enforcement_summaries.is_empty() {
            return false;
        }

        let prior_entries = self
            .task_repo()
            .list_activity(&task.id)
            .await
            .unwrap_or_default();
        let prior_refs: Vec<crate::tripwires::ActivityEntryRef> = prior_entries
            .iter()
            .map(crate::tripwires::ActivityEntryRef::from_entry)
            .collect();
        let carries = crate::tripwires::build_carry_forward_releases(
            &prior_refs,
            head_sha,
            &enforcement_summaries,
            &task.id,
            &task.project_id,
            Some(pull_number),
            &now,
        );

        let mut carried_fps: std::collections::HashSet<String> = std::collections::HashSet::new();
        for cf in &carries {
            for f in &cf.payload.released_findings {
                carried_fps.insert(f.content_fingerprint.clone());
            }
            let payload_json = serde_json::to_string(&cf.payload).unwrap_or_default();
            match self
                .task_repo()
                .log_activity(
                    Some(&task.id),
                    "coordinator",
                    "system",
                    crate::tripwires::TRIPWIRE_EVENT_HOLD_RELEASED,
                    &payload_json,
                )
                .await
            {
                Ok(_) => tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    head_sha = %head_sha,
                    from_head = %cf.from_head_sha,
                    released = cf.payload.released_findings.len(),
                    "PR poller: carried forward prior tripwire release to new head"
                ),
                Err(e) => tracing::warn!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    error = %e,
                    "PR poller: failed to log carry-forward tripwire.hold.released event"
                ),
            }
        }

        let all_carried = enforcement_summaries
            .iter()
            .all(|f| carried_fps.contains(&f.content_fingerprint));
        if all_carried && !carries.is_empty() {
            tracing::info!(
                task_id = %task.short_id,
                pr = pull_number,
                head_sha = %head_sha,
                "PR poller: tripwire gate HELD but all findings carried forward — proceeding"
            );
            true
        } else {
            false
        }
    }

    /// Create the tripwire hold for a gate that remains `Held` after the
    /// carry-forward pass.
    ///
    /// Resolution routing is governed by the per-rule
    /// [`Adjudication`](crate::tripwires::Adjudication) policy knob (default
    /// [`Arbiter`](crate::tripwires::Adjudication::Arbiter), NO human-review
    /// holds): a hold whose remaining enforcement findings all belong to
    /// arbiter-adjudicated rules is destined for autonomous arbiter
    /// adjudication; a hold that includes any rule an operator has explicitly
    /// opted to [`Human`](crate::tripwires::Adjudication::Human) takes the
    /// legacy human-review remediation path.
    ///
    /// The hold state itself — the `tripwire.gate.held` event (already logged
    /// by the caller), the active-hold state machine, the `human-review-hold`
    /// label, and merge blocking — is identical regardless of resolver; only
    /// the RESOLVER differs.
    ///
    /// NOTE: the autonomous arbiter-dispatch resolver (seed a tripwire
    /// arbitration row + route the source into the lead-intervention queue,
    /// where the arbiter `tripwire_release`s benign findings or reopens
    /// dangerous ones) is being wired as a follow-up. Until it lands, BOTH
    /// routing intents create the hold via the existing human-review
    /// remediation substrate so the gate stays fail-closed (a held PR never
    /// proceeds).
    pub(super) async fn create_tripwire_hold(
        &mut self,
        task: &djinn_core::models::Task,
        tripwire_result: &super::tripwire_gate::TripwireGateResult,
        head_sha: &str,
    ) {
        let policy = crate::tripwires::TripwirePolicy::default();
        let requires_human = tripwire_result
            .decision
            .findings
            .iter()
            .filter(|f| f.severity == crate::tripwires::TripwireFindingSeverity::EnforceHold)
            .any(|f| policy.adjudication_for(f.rule_id).is_human());

        let findings_summary = tripwire_result
            .decision
            .findings
            .iter()
            .filter(|f| f.severity == crate::tripwires::TripwireFindingSeverity::EnforceHold)
            .map(|f| {
                format!(
                    "- `{}` ({}) — {}",
                    f.rule_id.as_str(),
                    f.reason_code,
                    f.evidence.path,
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let hold_reason = format!(
            "Tripwire gate held for PR head `{}`.\n\n\
             Policy revision: `{}`\n\
             Enforcement findings:\n{}",
            &head_sha[..12.min(head_sha.len())],
            tripwire_result.decision.policy_revision,
            findings_summary,
        );

        tracing::info!(
            task_id = %task.short_id,
            head_sha = %head_sha,
            adjudication = if requires_human { "human" } else { "arbiter" },
            "PR poller: tripwire gate held — creating hold via remediation substrate"
        );
        self.create_remediation_task(
            &task.id,
            &hold_reason,
            &task.project_id,
            crate::dispatch::RemediationKind::HumanReview,
        )
        .await;
        self.park_source_open(&task.id, &hold_reason).await;
    }
}
