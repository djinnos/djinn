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
    /// holds):
    ///
    /// - **Arbiter (default)** — create an autonomous
    ///   [`PlannerEscalation`](crate::dispatch::RemediationKind::PlannerEscalation)
    ///   review task carrying the full adjudication dossier, and park the
    ///   source behind it. The Planner reviews each finding against the diff and
    ///   either CLOSES the escalation (releasing the hold via
    ///   [`releases_source_on_close`](crate::roles::releases_source_on_close) —
    ///   see [`emit_tripwire_release_on_hold_close`](CoordinatorActor::emit_tripwire_release_on_hold_close),
    ///   which releases every held head and unblocks the merge) or reopens the
    ///   source with a directive so the next push supersedes the hold. The
    ///   `human-review-hold` label is **deliberately NOT applied to the source**
    ///   on this path (that label only gates dispatch exclusion / auto-close
    ///   defense, neither wanted here). The merge is already blocked by the
    ///   active tripwire-hold state (`tripwire.gate.held` on the current head —
    ///   already logged by the caller); the label serves no purpose in the
    ///   autonomous flow, so the merge-boundary tamper reconciler
    ///   ([`reconcile_tripwire_hold`](CoordinatorActor::reconcile_tripwire_hold))
    ///   also skips re-applying it for arbiter-adjudicated holds.
    ///
    /// - **Human (org-policy escape hatch)** — a hold whose enforcement findings
    ///   include any rule an operator explicitly opted to
    ///   [`Human`](crate::tripwires::Adjudication::Human) takes the legacy
    ///   human-review remediation path (`human-review-hold`, awaits a human).
    pub(crate) async fn create_tripwire_hold(
        &mut self,
        task: &djinn_core::models::Task,
        tripwire_result: &super::tripwire_gate::TripwireGateResult,
        head_sha: &str,
    ) {
        self.create_tripwire_hold_with_policy(
            task,
            tripwire_result,
            head_sha,
            crate::tripwires::TripwirePolicy::default(),
        )
        .await;
    }

    /// Policy-injectable core of [`create_tripwire_hold`]. Production always
    /// passes [`TripwirePolicy::default`](crate::tripwires::TripwirePolicy::default)
    /// (org-policy loading is a follow-up); the `policy` seam lets tests
    /// exercise the human-adjudication escape hatch end-to-end.
    pub(crate) async fn create_tripwire_hold_with_policy(
        &mut self,
        task: &djinn_core::models::Task,
        tripwire_result: &super::tripwire_gate::TripwireGateResult,
        head_sha: &str,
        policy: crate::tripwires::TripwirePolicy,
    ) {
        let enforcement: Vec<&crate::tripwires::engine::TripwireFinding> = tripwire_result
            .decision
            .findings
            .iter()
            .filter(|f| f.severity == crate::tripwires::TripwireFindingSeverity::EnforceHold)
            .collect();
        let requires_human = enforcement
            .iter()
            .any(|f| policy.adjudication_for(f.rule_id).is_human());

        if requires_human {
            // ── Legacy human-review escape hatch ─────────────────────────────
            let findings_summary = enforcement
                .iter()
                .map(|f| {
                    format!(
                        "- `{}` ({}) — {}",
                        f.rule_id.as_str(),
                        f.reason_code,
                        f.evidence.path
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
                adjudication = "human",
                "PR poller: tripwire gate held — creating human-review hold (org-policy escape hatch)"
            );
            self.create_remediation_task(
                &task.id,
                &hold_reason,
                &task.project_id,
                crate::dispatch::RemediationKind::HumanReview,
            )
            .await;
            self.park_source_open(&task.id, &hold_reason).await;
            return;
        }

        // ── Arbiter-adjudicated (default): autonomous planner escalation ─────
        let dossier =
            build_tripwire_adjudication_dossier(task, tripwire_result, head_sha, &enforcement);
        tracing::info!(
            task_id = %task.short_id,
            head_sha = %head_sha,
            adjudication = "arbiter",
            enforcement_findings = enforcement.len(),
            "PR poller: tripwire gate held — creating autonomous planner-park escalation (no human hold, no source label)"
        );
        self.create_remediation_task(
            &task.id,
            &dossier,
            &task.project_id,
            crate::dispatch::RemediationKind::PlannerEscalation,
        )
        .await;
        self.park_source_open(&task.id, &dossier).await;
    }
}

/// Build the tripwire adjudication dossier that becomes the planner-park
/// escalation body: every enforcement finding (rule, reason code, evidence
/// path/span, content fingerprint), a per-file finding summary, the PR number +
/// held head SHA, and explicit close-releases / reopen-supersedes instructions.
fn build_tripwire_adjudication_dossier(
    task: &djinn_core::models::Task,
    tripwire_result: &super::tripwire_gate::TripwireGateResult,
    head_sha: &str,
    enforcement: &[&crate::tripwires::engine::TripwireFinding],
) -> String {
    let head12 = &head_sha[..12.min(head_sha.len())];
    let pr_display = tripwire_result
        .payload
        .pr_number
        .map(|n| format!("#{n}"))
        .unwrap_or_else(|| "(unknown)".to_owned());

    // Per-finding lines, with line-precise span when available.
    let findings_lines = enforcement
        .iter()
        .map(|f| {
            let span = match (f.evidence.start_line, f.evidence.end_line) {
                (Some(s), Some(e)) if s == e => format!(":{s}"),
                (Some(s), Some(e)) => format!(":{s}-{e}"),
                _ => String::new(),
            };
            format!(
                "- `{}` ({}) — {}{}  [fingerprint: {}]",
                f.rule_id.as_str(),
                f.reason_code,
                f.evidence.path,
                span,
                f.content_fingerprint,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Per-file summary (finding count per path) — a lightweight diffstat over
    // the flagged files (the deterministic gate does not carry line diffstat).
    let mut per_file: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for f in enforcement {
        *per_file.entry(f.evidence.path.as_str()).or_default() += 1;
    }
    let per_file_lines = per_file
        .iter()
        .map(|(path, count)| format!("- {path}: {count} finding(s)"))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "## Tripwire adjudication\n\n\
         The tripwire gate HELD PR {pr_display} at held head `{head12}` on {n} enforcement \
         finding(s). Policy revision: `{rev}`. Source task: {short_id}.\n\n\
         ### Findings\n{findings_lines}\n\n\
         ### Per-file summary\n{per_file_lines}\n\n\
         ### What to do\n\
         Review each finding against the diff.\n\
         - If BENIGN (code motion, refactor, workspace-internal dependency bumps, fixtures): \
         CLOSE this task with a rationale that NAMES the finding keys you cleared — closing \
         releases the hold and unblocks the merge.\n\
         - If genuinely DANGEROUS or unjustified: do NOT close-release. Reopen the source task \
         with a directive describing exactly what must change; the next push supersedes the hold.",
        pr_display = pr_display,
        head12 = head12,
        n = enforcement.len(),
        rev = tripwire_result.decision.policy_revision,
        short_id = task.short_id,
        findings_lines = findings_lines,
        per_file_lines = per_file_lines,
    )
}
