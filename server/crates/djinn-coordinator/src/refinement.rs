// djinn:allow-oversize — bounded refinement state machine and regression coverage exceed the byte threshold.
// Bounded proposal-refinement workflow state machine.
//
// Orchestrates Advocate revision, Adversary attack, and Judge adjudication
// in a bounded loop. The state-machine logic is unit-testable without
// spawning real agents; persistence goes through P2 debate-trail primitives
// and P1 deterministic readiness evaluation.
//
// Workflow:
//   1. Advocate revises the proposal (produces a proposal revision).
//   2. Adversary attacks the current revision (produces objections via debate trail).
//   3. If the adversary found blocking objections, loop back to step 1.
//   4. Repeat until `dry_rounds_required` consecutive rounds with no new blocking
//      objection ("dry rounds") — then invoke the Judge.
//   5. Judge adjudicates (produces a verdict via debate trail).
//   6. A blocking verdict re-opens the loop at the Adversary (step 2). If that
//      Adversary is dry, the round goes to the Advocate anyway so the verdict's
//      prescribed remedy is implemented — see `process_adversary_pass`.
//
// NOTE: `dry_rounds_required` is NOT enforced anywhere in the tree.
// `consecutive_dry_rounds` is incremented and reset but never compared against
// it, so step 4 above has never been true: a single dry pass invokes the Judge.
// Filed as a separate defect; do not read the constant as a live contract.
//
// Guards:
//   - Hard round cap (default 5).
//   - Bounded total agent spawn count.
//   - Repeated blocking objection-signature detection → escalation/stop.
//   - Deterministic stop reasons for every termination path.

use std::collections::HashMap;

use djinn_core::refinement_liveness::{RefinementRole, RefinementStopReason};

// ─── Constants ──────────────────────────────────────────────────────────────

/// Default hard round cap.
pub const DEFAULT_MAX_ROUNDS: i32 = 5;

/// Default consecutive dry adversary rounds required before invoking the Judge.
pub const DEFAULT_DRY_ROUNDS_REQUIRED: i32 = 2;

/// Default maximum total agent spawns across all rounds (advocate + adversary +
/// judge). Conservative: ~3 agents per round × 5 rounds + judge.
pub const DEFAULT_MAX_TOTAL_SPAWNS: i32 = 16;

/// Default threshold for repeated objection-signature escalation.
pub const DEFAULT_REPEAT_OBJECTION_THRESHOLD: usize = 2;

// ─── Public types ───────────────────────────────────────────────────────────

/// The state machine uses the shared durable stop contract directly. Keeping
/// this alias avoids a coordinator-local tag/context serialization boundary.
pub type StopReason = RefinementStopReason;

/// Convert the coordinator phase to the canonical durable role identity.
pub fn role_for_phase(phase: RefinementPhase) -> RefinementRole {
    match phase {
        RefinementPhase::AdversaryAttack => RefinementRole::Adversary,
        RefinementPhase::AdvocateRevision => RefinementRole::Advocate,
        RefinementPhase::JudgeAdjudication => RefinementRole::Judge,
        // Callers have already gated parks and terminal states before mapping
        // a role. There is no string fallback for an invalid phase.
        RefinementPhase::AwaitingHumanReview
        | RefinementPhase::Complete
        | RefinementPhase::AwaitingEvidence => unreachable!("non-dispatchable refinement phase"),
    }
}

/// The current phase of the refinement loop.
///
/// v2 tribunal order: every round runs `Adversary → Advocate → Judge`. The
/// Adversary opens (red-teams the current spec), the Advocate responds
/// (revises to address objections), and the Judge rules whether to loop again
/// or surface the result to the human. The human is NOT in the loop — they
/// only review the converged result once, via `AwaitingHumanReview`.
///
/// The Advocate step is skipped only when the Adversary is dry **and** the
/// Judge has no outstanding blocking verdict. A dry round that still owes the
/// Judge a remedy goes to the Advocate — see
/// [`RefinementLoopState::pending_blocking_verdict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefinementPhase {
    /// Waiting for the Adversary to red-team the current spec. (Round opener.)
    AdversaryAttack,
    /// Waiting for the Advocate to revise in response to the objections.
    AdvocateRevision,
    /// Waiting for the Judge to rule: continue, ready, or escalate.
    JudgeAdjudication,
    /// The tribunal converged (or hit caps). The refined spec is parked for a
    /// single human accept/reject; the autonomous loop is idle until resolved.
    AwaitingHumanReview,
    /// Refinement loop is complete.
    Complete,
    /// The Judge demanded evidence for a load-bearing spec claim. The loop
    /// is parked — no further tribunal phases are dispatched — until the
    /// linked evidence spike produces findings (resumed by the sibling epic
    /// oeqd). While parked, `drive_one_refinement` returns early without
    /// dispatching any new Adversary, Advocate, or Judge session.
    AwaitingEvidence,
}

/// An adversary objection as seen by the state machine. The coordinator
/// extracts these from agent session output and feeds them in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectionRecord {
    /// The body/summary of the objection.
    pub body: String,
    /// Whether this objection blocks proposal readiness.
    pub blocking: bool,
    /// The model that produced this objection.
    pub author_model: Option<String>,
    /// The debate-trail entry id (set after persistence).
    pub entry_id: Option<String>,
}

/// Result of an adversary attack pass.
#[derive(Debug, Clone, Default)]
pub struct AdversaryPassResult {
    /// Objections raised by the adversary.
    pub objections: Vec<ObjectionRecord>,
    /// True when the adversary explicitly signals no new objections (dry).
    pub explicit_dry: bool,
}

/// Result of an advocate revision pass.
#[derive(Debug, Clone)]
pub struct AdvocateRevisionResult {
    /// The new proposal title (same as current if only body changed).
    pub title: String,
    /// The new proposal body.
    pub body: String,
    /// The body format (markdown or mdx).
    pub body_format: String,
    /// The new acceptance criteria JSON array string.
    pub acceptance_criteria: String,
    /// Event metadata for the revision (e.g. block-catalog enrichment context).
    pub event_metadata: Option<serde_json::Value>,
}

/// One stable diagnostic returned when an Advocate's proposed write is
/// atomically rejected before it becomes a proposal revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvocateLintViolation {
    pub code: String,
    pub message: String,
    pub start_byte: u64,
    pub end_byte: u64,
}

/// Result of a judge adjudication pass.
#[derive(Debug, Clone)]
pub struct JudgeVerdictResult {
    /// The verdict body text.
    pub body: String,
    /// Whether the verdict blocks proposal readiness.
    pub blocking: bool,
}

/// Configuration for the refinement loop.
#[derive(Debug, Clone)]
pub struct RefinementConfig {
    /// Hard round cap (default 5).
    pub max_rounds: i32,
    /// Consecutive dry adversary rounds needed to stop (default 2).
    pub dry_rounds_required: i32,
    /// Maximum total agent spawns across all rounds.
    pub max_total_spawns: i32,
    /// Repeated objection signature threshold (default 2).
    pub repeat_objection_threshold: usize,
}

impl Default for RefinementConfig {
    fn default() -> Self {
        Self {
            max_rounds: DEFAULT_MAX_ROUNDS,
            dry_rounds_required: DEFAULT_DRY_ROUNDS_REQUIRED,
            max_total_spawns: DEFAULT_MAX_TOTAL_SPAWNS,
            repeat_objection_threshold: DEFAULT_REPEAT_OBJECTION_THRESHOLD,
        }
    }
}

/// The mutable state of an in-progress refinement loop. All fields are
/// primitives and `Copy`-able types so the state machine is cheap to clone
/// for snapshot/debug purposes.
#[derive(Debug, Clone)]
pub struct RefinementLoopState {
    /// Exact durable identity for this recoverable projection. Empty only for
    /// the legacy compatibility path while admission callers are cut over.
    pub run_id: String,
    /// Durable generation paired with `run_id`, used to fence late observations.
    pub generation: i32,
    /// The proposal being refined.
    pub proposal_id: String,
    /// Loop configuration.
    pub config: RefinementConfig,
    /// Current workflow phase.
    pub phase: RefinementPhase,
    /// 1-based round counter.
    pub current_round: i32,
    /// Consecutive adversary rounds that produced zero new blocking objections.
    pub consecutive_dry_rounds: i32,
    /// Total agent sessions spawned so far (advocate + adversary + judge).
    pub total_spawns: i32,
    /// The proposal revision seq the adversary should attack.
    /// Updated after each advocate revision.
    pub current_revision_seq: i32,
    /// The proposal's revision seq at the moment refinement started — the
    /// pre-refinement snapshot. If the human rejects the refined result, the
    /// live spec is reset back to this revision.
    pub snapshot_revision_seq: i32,
    /// The user this refinement run is attributed to: owner of the spawned
    /// refinement tasks (`created_by_user_id`) AND the scope used to resolve
    /// per-user role models for the tribunal agents. `None` means "fall back
    /// to the proposal author at dispatch time".
    pub attributed_user_id: Option<String>,
    /// Normalized objection-signature → occurrence count.
    pub objection_signatures: HashMap<String, usize>,
    /// Per-round blocking objection counts: `(round, count)`.
    pub round_blocking_objections: Vec<(i32, usize)>,
    /// Consecutive times a dispatched role session never actually ran (the
    /// task closed with zero session rows — e.g. a runtime/devcontainer setup
    /// failure). Reset to 0 whenever a session does run. Distinguishes "the
    /// adversary ran and was dry" from "the adversary never started", so a
    /// dispatch outage isn't silently treated as a dry round.
    pub dispatch_failures: i32,
    /// Diagnostics from the most recent rejected Advocate candidate. This is
    /// separate from proposal revisions: a rejected candidate did not change
    /// the head and must be corrected in the same refinement round.
    pub pending_advocate_lint_violations: Vec<AdvocateLintViolation>,
    /// Whether the Judge's latest verdict was blocking and no Advocate revision
    /// has answered it yet.
    ///
    /// A needs-work verdict prescribes a remedy, and only the Advocate can write
    /// it into the proposal body. But by the time the Judge rejects, it has
    /// usually resolved every objection the Adversary filed — so the next
    /// Adversary pass is legitimately dry, and a dry pass used to skip the
    /// Advocate and hand an unchanged spec straight back to the Judge. The
    /// remedy could then never be implemented by any role.
    ///
    /// This flag is what makes a dry pass route to the Advocate instead. It is
    /// derived state, not an independent source of truth: the durable authority
    /// is `ProposalRepository::latest_judge_verdict`, and both projection
    /// rebuild paths (`refinement_recovery`, `refinement_inflight`) rehydrate it
    /// from there via [`RefinementLoopState::with_pending_blocking_verdict`], so
    /// it survives a coordinator restart.
    pub pending_blocking_verdict: bool,
    /// Set when the loop terminates.
    pub stop_reason: Option<StopReason>,
}

impl RefinementLoopState {
    /// Create a new refinement loop for the given proposal.
    pub fn new(proposal_id: impl Into<String>, current_revision_seq: i32) -> Self {
        Self::with_config(
            proposal_id,
            current_revision_seq,
            RefinementConfig::default(),
        )
    }

    /// Create with explicit configuration.
    pub fn with_config(
        proposal_id: impl Into<String>,
        current_revision_seq: i32,
        config: RefinementConfig,
    ) -> Self {
        Self {
            run_id: String::new(),
            generation: 0,
            proposal_id: proposal_id.into(),
            config,
            phase: RefinementPhase::AdversaryAttack,
            current_round: 1,
            consecutive_dry_rounds: 0,
            total_spawns: 0,
            current_revision_seq,
            snapshot_revision_seq: current_revision_seq,
            attributed_user_id: None,
            objection_signatures: HashMap::new(),
            round_blocking_objections: Vec::new(),
            dispatch_failures: 0,
            pending_advocate_lint_violations: Vec::new(),
            pending_blocking_verdict: false,
            stop_reason: None,
        }
    }

    /// Attach the exact durable identity after loading an admitted run snapshot.
    pub fn with_run_identity(mut self, run_id: impl Into<String>, generation: i32) -> Self {
        self.run_id = run_id.into();
        self.generation = generation;
        self
    }

    /// Restore the pre-refinement snapshot revision captured when the run was
    /// admitted. Every projection rebuilt from the durable ledger must call
    /// this: `new` seeds the snapshot from the proposal head, which by then has
    /// advanced past the tribunal's own revisions, and the resulting projection
    /// would report the refined spec as its own `before` side. `None` (no
    /// durable start revision) leaves the head-derived seed in place.
    pub fn with_captured_snapshot_seq(mut self, snapshot_revision_seq: Option<i32>) -> Self {
        if let Some(seq) = snapshot_revision_seq.filter(|seq| *seq > 0) {
            self.snapshot_revision_seq = seq;
        }
        self
    }

    /// Rebuild an in-flight projection from the run's captured revision. The
    /// live proposal head can already contain the Advocate's output and cannot
    /// be used as the progress baseline.
    pub fn with_recovered_snapshot_seq(mut self, snapshot_revision_seq: Option<i32>) -> Self {
        if let Some(seq) = snapshot_revision_seq.filter(|seq| *seq > 0) {
            self.snapshot_revision_seq = seq;
            self.current_revision_seq = seq;
        }
        self
    }

    /// Rehydrate [`RefinementLoopState::pending_blocking_verdict`] from the
    /// durable debate trail. Every projection rebuilt from the ledger must call
    /// this: `new` seeds the flag `false`, so a run recovered while parked at
    /// `AdversaryAttack` after a needs-work verdict would send its next dry
    /// pass back to the Judge and re-strand the verdict the restart interrupted.
    ///
    /// The caller supplies `ProposalRepository::latest_judge_verdict(..).blocking`.
    /// That read is only consulted on the dry branch of `process_adversary_pass`
    /// — i.e. while the phase is `AdversaryAttack`, which is reachable only
    /// before any Advocate revision has answered the latest verdict — so the
    /// latest-verdict row is an exact witness at the point of use.
    pub fn with_pending_blocking_verdict(mut self, pending: bool) -> Self {
        self.pending_blocking_verdict = pending;
        self
    }

    /// Attribute this refinement run to a specific user (owner of the spawned
    /// refinement tasks + scope for per-user model resolution). Builder-style so
    /// the existing `new`/`with_config` constructors stay source-compatible.
    pub fn with_attributed_user(mut self, attributed_user_id: Option<String>) -> Self {
        self.attributed_user_id = attributed_user_id;
        self
    }

    /// Reconstruct a loop already parked in [`RefinementPhase::AwaitingHumanReview`]
    /// from durable lifecycle data after a server restart.
    ///
    /// The tribunal already converged (or was escalated) before the process
    /// died; the `refinement_awaiting_review` lifecycle row preserves the
    /// snapshot/refined revision seqs and the parked stop reason. This rebuilds
    /// just enough in-memory state that the human's single accept/reject review
    /// (`resolve_human_review`) and status derivation behave exactly as if the
    /// process had never restarted. No further tribunal phases are dispatched
    /// while parked, so the round/dry counters only need to be plausible for the
    /// reject-with-feedback re-loop; `current_round` is restored from the debate
    /// trail by the caller.
    pub fn restored_awaiting_review(
        proposal_id: impl Into<String>,
        refined_revision_seq: i32,
        snapshot_revision_seq: i32,
        current_round: i32,
        attributed_user_id: Option<String>,
        stop_reason: Option<StopReason>,
    ) -> Self {
        let mut state = Self::new(proposal_id, refined_revision_seq);
        state.phase = RefinementPhase::AwaitingHumanReview;
        state.snapshot_revision_seq = snapshot_revision_seq;
        state.current_round = current_round.max(1);
        state.attributed_user_id = attributed_user_id;
        state.stop_reason = stop_reason;
        state
    }

    /// Whether the loop has terminated.
    pub fn is_complete(&self) -> bool {
        self.phase == RefinementPhase::Complete
    }

    /// Whether the loop is parked awaiting the human's accept/reject review.
    /// While true, the coordinator dispatches no further tribunal phases.
    pub fn is_awaiting_human_review(&self) -> bool {
        self.phase == RefinementPhase::AwaitingHumanReview
    }

    /// Whether the loop is parked waiting for an evidence spike to produce
    /// findings. While true, the coordinator dispatches no further tribunal
    /// phases.
    pub fn is_awaiting_evidence(&self) -> bool {
        self.phase == RefinementPhase::AwaitingEvidence
    }

    /// Record that an agent session was spawned. Returns `Err` if the spawn
    /// cap would be exceeded.
    pub fn record_spawn(&mut self) -> Result<(), StopReason> {
        self.total_spawns += 1;
        if self.total_spawns > self.config.max_total_spawns {
            let reason = if self.phase == RefinementPhase::AdvocateRevision
                && !self.pending_advocate_lint_violations.is_empty()
            {
                StopReason::AgentFailure {
                    role: RefinementRole::Advocate,
                    error_code: "spec_lint_rejected".into(),
                    message: "SPEC_LINT_REJECTED persisted until the established session/spawn cap"
                        .into(),
                }
            } else {
                StopReason::SpawnCap
            };
            self.terminate(reason.clone());
            Err(reason)
        } else {
            Ok(())
        }
    }

    /// Record an advocate revision. Advances the working revision seq and
    /// hands the round to the Judge to rule.
    ///
    /// The revision answers whatever the Judge's last blocking verdict
    /// prescribed, so the outstanding-verdict flag is cleared here: the Judge is
    /// about to rule on the new revision, and that ruling — not this one — is
    /// what decides whether a remedy is still owed.
    pub fn record_advocate_revision(&mut self, new_revision_seq: i32) {
        self.current_revision_seq = new_revision_seq;
        self.pending_advocate_lint_violations.clear();
        self.pending_blocking_verdict = false;
        self.phase = RefinementPhase::JudgeAdjudication;
    }

    /// Preserve the same round and revision sequence after a rejected candidate
    /// so the normal bounded session/spawn accounting dispatches a correction.
    pub fn record_advocate_lint_rejection(&mut self, violations: Vec<AdvocateLintViolation>) {
        self.pending_advocate_lint_violations = violations;
        self.phase = RefinementPhase::AdvocateRevision;
    }

    /// Process an adversary attack pass — the round opener. Records objections
    /// for repeat-signature detection, then routes:
    /// - blocking objections found → the Advocate revises to address them;
    /// - no blocking objections (dry) **with an outstanding blocking judge
    ///   verdict** → the Advocate still runs, to implement the remedy that
    ///   verdict prescribed (see [`RefinementLoopState::pending_blocking_verdict`]);
    /// - no blocking objections (dry) and nothing owed to the Judge → skip the
    ///   Advocate (nothing to fix) and let the Judge rule directly;
    /// - the same blocking objection repeating across rounds → the loop is
    ///   stuck, so escalate to the human reviewer.
    ///
    /// Dry accounting is identical in both dry branches: a dry pass is a dry
    /// pass regardless of what the Judge left outstanding, and the reported
    /// outcome stays [`AdversaryPassOutcome::Dry`] because it describes the
    /// Adversary's own output, not the phase that follows.
    ///
    /// Pure state-machine logic — the caller persists objections through the
    /// debate-trail primitives.
    pub fn process_adversary_pass(&mut self, result: &AdversaryPassResult) -> AdversaryPassOutcome {
        let blocking: Vec<_> = result.objections.iter().filter(|o| o.blocking).collect();
        let blocking_count = blocking.len();

        self.round_blocking_objections
            .push((self.current_round, blocking_count));
        for obj in &blocking {
            let sig = normalize_objection_signature(&obj.body);
            *self.objection_signatures.entry(sig).or_insert(0) += 1;
        }

        // Same objection going in circles → the tribunal is stuck; surface it.
        if let Some((sig, count)) = self.detect_repeated_signature() {
            let reason = StopReason::RepeatedObjection {
                signature: sig,
                occurrences: count as u64,
            };
            self.escalate(reason.clone());
            return AdversaryPassOutcome::Escalated(reason);
        }

        if blocking_count == 0 || result.explicit_dry {
            self.consecutive_dry_rounds += 1;
            // A dry Adversary does NOT mean there is nothing to revise. When the
            // Judge's last verdict was blocking, it prescribed a remedy that only
            // the Advocate can write into the body — and by then the Judge has
            // usually resolved every objection the Adversary filed, so the
            // Adversary legitimately has nothing left to raise. Routing straight
            // to the Judge here is what stranded those verdicts forever: the
            // Judge re-read an unchanged spec, rejected it again, and the round
            // burned. Give the Advocate the round instead.
            self.phase = if self.pending_blocking_verdict {
                RefinementPhase::AdvocateRevision
            } else {
                // Genuinely nothing owed — straight to the judge.
                RefinementPhase::JudgeAdjudication
            };
            return AdversaryPassOutcome::Dry;
        }

        self.consecutive_dry_rounds = 0;
        self.phase = RefinementPhase::AdvocateRevision;
        AdversaryPassOutcome::Continue
    }

    /// Record a judge verdict — the round's arbiter.
    /// - non-blocking ("ready") → the spec converged; park it for the human's
    ///   single accept/reject review;
    /// - blocking ("not ready") → run another round (Adversary first), unless
    ///   the round cap is hit, in which case escalate to the human.
    ///
    /// A blocking verdict also records that the Judge left a remedy outstanding
    /// ([`RefinementLoopState::pending_blocking_verdict`]). The round still
    /// opens at the Adversary — every revision stays adversarially reviewed and
    /// the round keeps its three-spawn shape — but if that Adversary comes up
    /// dry, `process_adversary_pass` hands the round to the Advocate instead of
    /// bouncing an unchanged spec back to the Judge.
    pub fn record_judge_verdict(&mut self, result: &JudgeVerdictResult) {
        if !result.blocking {
            // A ready verdict supersedes any earlier needs-work verdict.
            self.pending_blocking_verdict = false;
            self.phase = RefinementPhase::AwaitingHumanReview;
            return;
        }
        self.pending_blocking_verdict = true;
        if self.current_round >= self.config.max_rounds {
            self.escalate(StopReason::RoundCap);
            return;
        }
        self.current_round += 1;
        self.phase = RefinementPhase::AdversaryAttack;
    }

    /// Record that the Judge demanded evidence for a load-bearing claim.
    /// Parks the loop in `AwaitingEvidence` — no further tribunal phases are
    /// dispatched until the linked evidence spike produces findings. Does NOT
    /// advance the round counter; the round stays at the value where the
    /// demand was issued so the resumed Judge session can complete its
    /// adjudication at that same round boundary.
    pub fn record_needs_evidence(&mut self) {
        self.phase = RefinementPhase::AwaitingEvidence;
    }

    /// Resume after a linked evidence spike has produced valid findings. The
    /// next tribunal action must be the Advocate so the findings can be used to
    /// answer the current objections before Judge adjudication continues.
    pub fn resume_after_evidence_received(&mut self) {
        self.phase = RefinementPhase::AdvocateRevision;
    }

    /// Park the loop for human review (converged or escalated). Records the
    /// reason for display but does NOT complete — the human still acts.
    pub fn escalate(&mut self, reason: StopReason) {
        self.stop_reason = Some(reason);
        self.phase = RefinementPhase::AwaitingHumanReview;
    }

    /// Apply the human's decision on the parked review.
    /// - `accept` → loop Complete; the refined spec is kept.
    /// - `reject` WITH feedback → re-open the tribunal with the feedback as a
    ///   fresh top-priority objection (new round, Adversary first).
    /// - `reject` WITHOUT feedback → loop Complete; the caller reverts the live
    ///   spec to the pre-refinement snapshot.
    pub fn resolve_human_review(&mut self, accept: bool, has_feedback: bool) {
        if accept {
            self.terminate(StopReason::HumanAccepted);
        } else if has_feedback {
            self.stop_reason = None;
            self.consecutive_dry_rounds = 0;
            self.current_round += 1;
            self.phase = RefinementPhase::AdversaryAttack;
        } else {
            self.terminate(StopReason::HumanRejected);
        }
    }

    /// Terminate the loop with the given stop reason.
    pub fn terminate(&mut self, reason: StopReason) {
        self.stop_reason = Some(reason);
        self.phase = RefinementPhase::Complete;
    }

    /// Detect whether any blocking objection signature has appeared
    /// `config.repeat_objection_threshold` or more times.
    fn detect_repeated_signature(&self) -> Option<(String, usize)> {
        for (sig, count) in &self.objection_signatures {
            if *count >= self.config.repeat_objection_threshold {
                return Some((sig.clone(), *count));
            }
        }
        None
    }
}

/// Outcome of processing an adversary pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdversaryPassOutcome {
    /// No escalation; the loop continues to the next round.
    Continue,
    /// The adversary is dry; the Judge should be invoked.
    Dry,
    /// The loop was escalated/terminated.
    Escalated(StopReason),
}

// ─── Objection signature normalization ──────────────────────────────────────

/// Normalize an objection body into a canonical signature for repeat-detection.
///
/// The normalization is intentionally lossy: it collapses whitespace, lowercases,
/// strips leading/trailing punctuation, and truncates to 200 chars so that
/// semantically identical objections with minor wording differences match.
pub fn normalize_objection_signature(body: &str) -> String {
    let lowered = body.to_lowercase();
    let collapsed: String = lowered.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace());
    // Truncate to keep signature storage bounded. Cut on a char boundary:
    // a byte-index slice panics when byte 200 lands inside a multi-byte
    // char (e.g. a curly quote in an objection body), killing the actor.
    match trimmed.char_indices().nth(200) {
        Some((byte_idx, _)) => trimmed[..byte_idx].to_string(),
        None => trimmed.to_string(),
    }
}

// ─── Diverse-refinement model selection ─────────────────────────────────────

/// Select a model for a refinement role using the best-effort cross-model
/// diversity pattern from `diverse_refinement` user settings.
///
/// When `diverse_refinement` is enabled and the `candidates` list contains a
/// model different from `primary_model`, the first such model is preferred.
/// When no alternative exists (degenerate single-model selection), the primary
/// model is returned rather than blocking execution. The same-model fallback
/// is logged/metric'd by the caller.
///
/// This mirrors the `apply_diverse_review_ordering` pattern in task dispatch
/// but operates on an explicit candidate list rather than mutating a slice.
pub fn select_refinement_model(
    diverse_refinement: bool,
    primary_model: &str,
    candidates: &[String],
) -> (String, bool) {
    if !diverse_refinement || candidates.is_empty() {
        return (primary_model.to_string(), false);
    }

    let has_alternative = candidates.iter().any(|m| m != primary_model);
    if !has_alternative {
        // Same-model fallback: log/metric by caller.
        return (primary_model.to_string(), true);
    }

    // Prefer the first candidate that differs from the primary model.
    let selected = candidates
        .iter()
        .find(|m| *m != primary_model)
        .cloned()
        .unwrap_or_else(|| primary_model.to_string());
    (selected, false)
}

// ─── Debate-trail entry builders ────────────────────────────────────────────

/// Build the `event_metadata` JSON for an advocate revision attributed to the
/// refinement loop. This is persisted in `proposal_revisions.event_metadata`
/// so revisions are attributable by role and round, and so the proposal
/// history UI can collapse a whole refinement run (all `source ==
/// "refinement_loop"` revisions) into a single "Refined via tribunal" entry.
pub fn build_revision_event_metadata(round: i32, author_model: Option<&str>) -> serde_json::Value {
    let mut meta = serde_json::json!({
        "source": "refinement_loop",
        "role": "advocate",
        "round": round,
    });
    if let Some(model) = author_model {
        meta["author_model"] = serde_json::Value::String(model.to_string());
    }
    meta
}

/// Build debate-trail create input parameters for an adversary objection.
/// The caller is responsible for calling `ProposalRepository::add_debate_trail_entry`.
pub fn build_objection_debate_params<'a>(
    proposal_id: &'a str,
    objection: &'a ObjectionRecord,
    against_revision_seq: i32,
    round: i32,
    author_model: Option<&'a str>,
) -> djinn_db::ProposalDebateTrailCreateInput<'a> {
    djinn_db::ProposalDebateTrailCreateInput {
        proposal_id,
        kind: "objection",
        body: &objection.body,
        blocking: objection.blocking,
        agent_role: "adversary",
        author_kind: "agent",
        author_model,
        source_task_id: None,
        against_revision_seq,
        round,
        body_metadata: None,
    }
}

/// Build debate-trail create input parameters for a judge verdict.
pub fn build_verdict_debate_params<'a>(
    proposal_id: &'a str,
    verdict: &'a JudgeVerdictResult,
    against_revision_seq: i32,
    round: i32,
    author_model: Option<&'a str>,
) -> djinn_db::ProposalDebateTrailCreateInput<'a> {
    djinn_db::ProposalDebateTrailCreateInput {
        proposal_id,
        kind: "verdict",
        body: &verdict.body,
        blocking: verdict.blocking,
        agent_role: "judge",
        author_kind: "agent",
        author_model,
        source_task_id: None,
        against_revision_seq,
        round,
        body_metadata: None,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> RefinementConfig {
        RefinementConfig {
            max_rounds: 5,
            dry_rounds_required: 2,
            max_total_spawns: 16,
            repeat_objection_threshold: 2,
        }
    }

    fn blocking_objection(body: &str) -> ObjectionRecord {
        ObjectionRecord {
            body: body.into(),
            blocking: true,
            author_model: None,
            entry_id: None,
        }
    }

    // ── Start state ──────────────────────────────────────────────────────

    #[test]
    fn starts_in_adversary_attack_phase() {
        let state = RefinementLoopState::new("p1", 7);
        assert_eq!(state.phase, RefinementPhase::AdversaryAttack);
        assert_eq!(state.current_round, 1);
        assert_eq!(state.current_revision_seq, 7);
        // The pre-refinement snapshot baseline equals the starting revision.
        assert_eq!(state.snapshot_revision_seq, 7);
        assert!(!state.is_complete());
        assert!(!state.is_awaiting_human_review());
    }

    // ── Adversary routing ────────────────────────────────────────────────

    #[test]
    fn adversary_blocking_objections_route_to_advocate_revision() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![blocking_objection("Missing problem section")],
            explicit_dry: false,
        });
        assert_eq!(outcome, AdversaryPassOutcome::Continue);
        assert_eq!(state.phase, RefinementPhase::AdvocateRevision);
        assert_eq!(state.consecutive_dry_rounds, 0);
        // The adversary opener does not advance the round; the judge does.
        assert_eq!(state.current_round, 1);
    }

    #[test]
    fn adversary_dry_routes_straight_to_judge() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![],
            explicit_dry: true,
        });
        assert_eq!(outcome, AdversaryPassOutcome::Dry);
        assert_eq!(state.phase, RefinementPhase::JudgeAdjudication);
        assert_eq!(state.consecutive_dry_rounds, 1);
    }

    #[test]
    fn adversary_no_blocking_objections_is_dry() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        // Only non-blocking objections — nothing for the advocate to fix.
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![ObjectionRecord {
                body: "Minor style nit".into(),
                blocking: false,
                author_model: None,
                entry_id: None,
            }],
            explicit_dry: false,
        });
        assert_eq!(outcome, AdversaryPassOutcome::Dry);
        assert_eq!(state.phase, RefinementPhase::JudgeAdjudication);
    }

    // ── Advocate routing ─────────────────────────────────────────────────

    #[test]
    fn advocate_revision_routes_to_judge() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        // Adversary raised a blocking objection → advocate revises.
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![blocking_objection("Missing scope")],
            explicit_dry: false,
        });
        assert_eq!(state.phase, RefinementPhase::AdvocateRevision);

        state.record_advocate_revision(3);
        assert_eq!(state.phase, RefinementPhase::JudgeAdjudication);
        assert_eq!(state.current_revision_seq, 3);
        // Advocate revision does not advance the round.
        assert_eq!(state.current_round, 1);
    }

    // ── Judge routing ────────────────────────────────────────────────────

    #[test]
    fn judge_non_blocking_parks_for_human_review() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![],
            explicit_dry: true,
        });
        state.record_judge_verdict(&JudgeVerdictResult {
            body: "Proposal is ready.".into(),
            blocking: false,
        });
        assert_eq!(state.phase, RefinementPhase::AwaitingHumanReview);
        assert!(state.is_awaiting_human_review());
        assert!(!state.is_complete());
    }

    #[test]
    fn judge_blocking_starts_next_adversary_round() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![blocking_objection("Issue A")],
            explicit_dry: false,
        });
        state.record_advocate_revision(1);
        // Judge still finds the spec lacking → next round, adversary first, so
        // the revision that was just rejected is still red-teamed.
        state.record_judge_verdict(&JudgeVerdictResult {
            body: "Still needs work.".into(),
            blocking: true,
        });
        assert_eq!(state.phase, RefinementPhase::AdversaryAttack);
        assert_eq!(state.current_round, 2);
        assert!(!state.is_complete());
        // The remedy the verdict prescribed is recorded as still owed.
        assert!(state.pending_blocking_verdict);
    }

    /// The wedge. A blocking verdict prescribes a remedy only the Advocate can
    /// write, and by the time the Judge rejects it has usually resolved every
    /// objection the Adversary filed — so the next Adversary pass is genuinely
    /// dry. A dry pass used to skip the Advocate and hand the unchanged spec
    /// straight back to the Judge, which re-rejected it. The remedy could never
    /// be implemented by any role, and the run burned rounds to the cap.
    ///
    /// A dry pass that still owes the Judge a remedy must reach the Advocate.
    #[test]
    fn dry_adversary_with_an_outstanding_verdict_routes_to_the_advocate() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());

        // Round 1: the Adversary objects, the Advocate revises, the Judge
        // rejects with a prescribed remedy and resolves the objection.
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![blocking_objection("Missing rollback path")],
            explicit_dry: false,
        });
        state.record_advocate_revision(1);
        state.record_judge_verdict(&JudgeVerdictResult {
            body: "needs-work: AC 3 is untestable; replace it with a measurable \
                   assertion on the emitted row count."
                .into(),
            blocking: true,
        });
        assert_eq!(state.phase, RefinementPhase::AdversaryAttack);
        assert_eq!(state.current_round, 2);

        // Round 2: the Adversary has nothing left to raise — every objection it
        // filed is resolved. This is the exact state that used to dead-end.
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![],
            explicit_dry: true,
        });

        assert_eq!(
            outcome,
            AdversaryPassOutcome::Dry,
            "the pass is still dry — only the phase it routes to changes"
        );
        assert_eq!(
            state.phase,
            RefinementPhase::AdvocateRevision,
            "a dry pass owing the Judge a remedy must reach the only role that \
             can revise the body"
        );
        assert!(!state.is_complete());
        assert!(!state.is_awaiting_human_review());
        assert!(state.stop_reason.is_none());
        // Dry accounting is untouched by the reroute.
        assert_eq!(state.consecutive_dry_rounds, 1);
        assert_eq!(
            state.current_round, 2,
            "the reroute does not advance the round"
        );

        // The Advocate's revision answers the verdict and hands the round back
        // to the Judge, so the loop keeps moving.
        state.record_advocate_revision(2);
        assert_eq!(state.phase, RefinementPhase::JudgeAdjudication);
        assert!(
            !state.pending_blocking_verdict,
            "a revision answers the outstanding verdict"
        );
    }

    /// A dry pass with NOTHING owed must still go straight to the Judge. The
    /// reroute is conditional, not a blanket "dry means advocate".
    #[test]
    fn dry_adversary_without_an_outstanding_verdict_still_goes_to_the_judge() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        assert!(!state.pending_blocking_verdict);

        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![],
            explicit_dry: true,
        });
        assert_eq!(state.phase, RefinementPhase::JudgeAdjudication);

        // A ready verdict clears the flag, so a later dry pass is not rerouted
        // by a stale needs-work from an earlier round.
        state.pending_blocking_verdict = true;
        state.record_judge_verdict(&JudgeVerdictResult {
            body: "Ready.".into(),
            blocking: false,
        });
        assert!(
            !state.pending_blocking_verdict,
            "a ready verdict supersedes any earlier needs-work"
        );
    }

    /// The outstanding-verdict bit is rebuilt from the durable debate trail, so
    /// a run recovered mid-round does not re-strand the verdict the restart
    /// interrupted.
    #[test]
    fn rehydrated_outstanding_verdict_reroutes_a_dry_pass_after_a_restart() {
        // A projection rebuilt from the ledger: phase and round come from the
        // intent, the outstanding-verdict bit from `latest_judge_verdict`.
        let mut state = RefinementLoopState::with_config("p1", 4, test_config())
            .with_pending_blocking_verdict(true);
        state.phase = RefinementPhase::AdversaryAttack;
        state.current_round = 3;

        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![],
            explicit_dry: true,
        });
        assert_eq!(
            state.phase,
            RefinementPhase::AdvocateRevision,
            "a recovered run must honor the verdict the trail still shows outstanding"
        );

        // The same projection without the rehydration falls back to the
        // pre-existing routing — this is what the recovery read prevents.
        let mut unrehydrated = RefinementLoopState::with_config("p1", 4, test_config());
        unrehydrated.phase = RefinementPhase::AdversaryAttack;
        unrehydrated.current_round = 3;
        unrehydrated.process_adversary_pass(&AdversaryPassResult {
            objections: vec![],
            explicit_dry: true,
        });
        assert_eq!(unrehydrated.phase, RefinementPhase::JudgeAdjudication);
    }

    /// The reroute must not cost extra agent spawns. `SpawnCap` *terminates* a
    /// run; `RoundCap` *parks* it for human review. If a dry-with-verdict round
    /// ever costs more than the round it replaces, `SpawnCap` preempts
    /// `RoundCap` and a reviewable park silently becomes a dead run.
    ///
    /// The spawn count here is DERIVED from the phases the state machine
    /// actually asks for — one agent per dispatchable phase — rather than a
    /// schedule this test hard-codes. A change that visits an extra phase per
    /// round therefore blows the budget and trips `record_spawn`, instead of
    /// passing because the test's own model was updated to match.
    #[test]
    fn worst_case_dry_verdict_run_parks_on_the_round_cap_not_the_spawn_cap() {
        let config = test_config();
        let mut state = RefinementLoopState::with_config("p1", 0, config.clone());
        let mut visited: Vec<RefinementPhase> = Vec::new();
        let mut revision = 0;

        // Worst case for this fix: the Judge rejects every round, and the
        // Adversary is dry every round because the Judge resolved its
        // objections. Bounded by the round cap; the guard below is the budget.
        while !state.is_awaiting_human_review() && !state.is_complete() {
            assert!(
                visited.len() < 64,
                "the loop must terminate through a cap, not run forever"
            );
            state
                .record_spawn()
                .unwrap_or_else(|reason| panic!("spawn budget exhausted at {reason:?}"));
            visited.push(state.phase);
            match state.phase {
                RefinementPhase::AdversaryAttack => {
                    state.process_adversary_pass(&AdversaryPassResult {
                        objections: vec![],
                        explicit_dry: true,
                    });
                }
                RefinementPhase::AdvocateRevision => {
                    revision += 1;
                    state.record_advocate_revision(revision);
                }
                RefinementPhase::JudgeAdjudication => {
                    state.record_judge_verdict(&JudgeVerdictResult {
                        body: format!("needs-work: round {}", state.current_round),
                        blocking: true,
                    });
                }
                other => panic!("non-dispatchable phase reached the dispatch loop: {other:?}"),
            }
        }

        // Parked for a human on the ROUND cap — not terminated on the spawn cap.
        assert_eq!(state.stop_reason, Some(StopReason::RoundCap));
        assert!(state.is_awaiting_human_review());
        assert!(!state.is_complete());
        assert_eq!(state.current_round, config.max_rounds);
        assert!(
            visited.len() as i32 <= config.max_total_spawns,
            "worst-case run asked for {} spawns, budget is {}: {visited:?}",
            visited.len(),
            config.max_total_spawns
        );

        // And the reroute actually happened: the Advocate ran on the dry rounds
        // that owed the Judge a remedy (every round after the first).
        let advocate_turns = visited
            .iter()
            .filter(|phase| **phase == RefinementPhase::AdvocateRevision)
            .count();
        assert_eq!(
            advocate_turns,
            (config.max_rounds - 1) as usize,
            "every dry round after the first owes a verdict and must run the \
             Advocate: {visited:?}"
        );
    }

    #[test]
    fn judge_blocking_at_round_cap_escalates_to_human_review() {
        let config = RefinementConfig {
            max_rounds: 2,
            ..test_config()
        };
        let mut state = RefinementLoopState::with_config("p1", 0, config);

        // Round 1: adversary blocking → advocate → judge blocking → round 2.
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![blocking_objection("Issue A")],
            explicit_dry: false,
        });
        state.record_advocate_revision(1);
        state.record_judge_verdict(&JudgeVerdictResult {
            body: "Needs work.".into(),
            blocking: true,
        });
        assert_eq!(state.current_round, 2);
        assert_eq!(state.phase, RefinementPhase::AdversaryAttack);

        // Round 2 (== cap): adversary blocking → advocate → judge blocking →
        // escalate to human review (round cap reached).
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![blocking_objection("Issue B")],
            explicit_dry: false,
        });
        state.record_advocate_revision(2);
        state.record_judge_verdict(&JudgeVerdictResult {
            body: "Still needs work.".into(),
            blocking: true,
        });
        assert!(state.is_awaiting_human_review());
        assert!(!state.is_complete());
        assert_eq!(state.stop_reason, Some(StopReason::RoundCap));
    }

    // ── Repeated objection escalation ────────────────────────────────────

    #[test]
    fn repeated_blocking_objection_escalates_to_human_review() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());

        // Round 1: blocking objection → advocate revision.
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![blocking_objection("Missing Problem Statement")],
            explicit_dry: false,
        });
        assert_eq!(outcome, AdversaryPassOutcome::Continue);
        state.record_advocate_revision(1);
        state.record_judge_verdict(&JudgeVerdictResult {
            body: "Not ready.".into(),
            blocking: true,
        });
        assert_eq!(state.current_round, 2);
        assert_eq!(state.phase, RefinementPhase::AdversaryAttack);

        // Round 2: the same objection (different casing/whitespace) repeats →
        // the tribunal is stuck, escalate to the human. Reachable on every
        // judge-rejected round because those rounds still open at the Adversary.
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![blocking_objection("missing  problem  statement")],
            explicit_dry: false,
        });
        assert!(matches!(
            outcome,
            AdversaryPassOutcome::Escalated(StopReason::RepeatedObjection { .. })
        ));
        // Escalation parks for human review — it does NOT complete the loop.
        assert!(state.is_awaiting_human_review());
        assert!(!state.is_complete());

        if let Some(StopReason::RepeatedObjection {
            signature,
            occurrences,
        }) = &state.stop_reason
        {
            assert_eq!(signature, "missing problem statement");
            assert_eq!(*occurrences, 2);
        } else {
            panic!("expected RepeatedObjection stop reason");
        }
    }

    #[test]
    fn non_blocking_objections_do_not_trigger_repeat_escalation() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());

        // Two non-blocking objections with identical bodies across rounds —
        // non-blocking objections never count toward repeat detection.
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![ObjectionRecord {
                body: "Minor style concern".into(),
                blocking: false,
                author_model: None,
                entry_id: None,
            }],
            explicit_dry: false,
        });
        // No blocking objections → dry round, straight to the judge.
        assert_eq!(outcome, AdversaryPassOutcome::Dry);
        assert!(!state.is_awaiting_human_review());
        assert!(!state.is_complete());
    }

    // ── Human review resolution ──────────────────────────────────────────

    #[test]
    fn human_accept_completes_loop() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        state.escalate(StopReason::RoundCap);
        assert!(state.is_awaiting_human_review());

        state.resolve_human_review(true, true);
        assert!(state.is_complete());
        assert_eq!(state.stop_reason, Some(StopReason::HumanAccepted));
    }

    #[test]
    fn human_reject_with_feedback_starts_new_round() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        // Converge first: judge parks for review at round 1.
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![],
            explicit_dry: true,
        });
        state.record_judge_verdict(&JudgeVerdictResult {
            body: "Ready.".into(),
            blocking: false,
        });
        assert!(state.is_awaiting_human_review());

        // Reject WITH feedback → re-open the tribunal, adversary first.
        state.resolve_human_review(false, true);
        assert_eq!(state.phase, RefinementPhase::AdversaryAttack);
        assert_eq!(state.current_round, 2);
        assert!(state.stop_reason.is_none());
        assert!(!state.is_complete());
        assert!(!state.is_awaiting_human_review());
    }

    #[test]
    fn human_reject_without_feedback_completes_loop() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        state.escalate(StopReason::AdversaryDry);
        assert!(state.is_awaiting_human_review());

        // Reject WITHOUT feedback → terminate; caller reverts to snapshot.
        state.resolve_human_review(false, false);
        assert!(state.is_complete());
        assert_eq!(state.stop_reason, Some(StopReason::HumanRejected));
    }

    // ── End-to-end happy path ────────────────────────────────────────────

    #[test]
    fn full_round_converges_to_human_review() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        assert_eq!(state.phase, RefinementPhase::AdversaryAttack);

        // Adversary blocking → advocate revises → judge approves → human review.
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![blocking_objection("Missing acceptance criteria")],
            explicit_dry: false,
        });
        assert_eq!(state.phase, RefinementPhase::AdvocateRevision);

        state.record_advocate_revision(1);
        assert_eq!(state.phase, RefinementPhase::JudgeAdjudication);

        state.record_judge_verdict(&JudgeVerdictResult {
            body: "Looks good now.".into(),
            blocking: false,
        });
        assert!(state.is_awaiting_human_review());

        state.resolve_human_review(true, false);
        assert!(state.is_complete());
        assert_eq!(state.stop_reason, Some(StopReason::HumanAccepted));
    }

    // ── Total spawn cap ──────────────────────────────────────────────────

    #[test]
    fn enforces_total_spawn_cap() {
        let config = RefinementConfig {
            max_total_spawns: 3,
            ..test_config()
        };
        let mut state = RefinementLoopState::with_config("p1", 0, config);

        assert!(state.record_spawn().is_ok());
        assert!(state.record_spawn().is_ok());
        assert!(state.record_spawn().is_ok());

        let err = state.record_spawn().unwrap_err();
        assert_eq!(err, StopReason::SpawnCap);
        assert!(state.is_complete());
        assert_eq!(state.stop_reason, Some(StopReason::SpawnCap));
    }

    // ── Agent failure termination ────────────────────────────────────────

    #[test]
    fn agent_failure_terminates_loop() {
        let mut state = RefinementLoopState::new("p1", 0);
        state.terminate(StopReason::AgentFailure {
            role: RefinementRole::Adversary,
            error_code: "session_crashed".into(),
            message: "session crashed".into(),
        });
        assert!(state.is_complete());
        assert!(matches!(
            state.stop_reason,
            Some(StopReason::AgentFailure { .. })
        ));
    }

    // ── Diverse-refinement model selection ───────────────────────────────

    #[test]
    fn diverse_refinement_selects_alternative_model() {
        let candidates = vec![
            "openai/gpt-4o".to_string(),
            "anthropic/claude-sonnet-4-20250514".to_string(),
        ];
        let (model, same_fallback) = select_refinement_model(true, "openai/gpt-4o", &candidates);
        assert_eq!(model, "anthropic/claude-sonnet-4-20250514");
        assert!(!same_fallback);
    }

    #[test]
    fn diverse_refinement_falls_back_to_same_model() {
        let candidates = vec!["openai/gpt-4o".to_string()];
        let (model, same_fallback) = select_refinement_model(true, "openai/gpt-4o", &candidates);
        assert_eq!(model, "openai/gpt-4o");
        assert!(same_fallback, "must flag same-model fallback");
    }

    #[test]
    fn diverse_refinement_disabled_returns_primary() {
        let candidates = vec![
            "openai/gpt-4o".to_string(),
            "anthropic/claude-sonnet-4-20250514".to_string(),
        ];
        let (model, same_fallback) = select_refinement_model(false, "openai/gpt-4o", &candidates);
        assert_eq!(model, "openai/gpt-4o");
        assert!(!same_fallback);
    }

    #[test]
    fn diverse_refinement_empty_candidates_returns_primary() {
        let (model, same_fallback) = select_refinement_model(true, "openai/gpt-4o", &[]);
        assert_eq!(model, "openai/gpt-4o");
        assert!(!same_fallback);
    }

    // ── Objection signature normalization ────────────────────────────────

    #[test]
    fn normalization_collapses_whitespace_and_lowercases() {
        assert_eq!(
            normalize_objection_signature("Missing  Problem   Statement"),
            "missing problem statement"
        );
    }

    #[test]
    fn normalization_strips_punctuation() {
        assert_eq!(
            normalize_objection_signature("!!!Missing Problem Statement!!!"),
            "missing problem statement"
        );
    }

    #[test]
    fn normalization_truncates_long_bodies() {
        let long_body = "a".repeat(500);
        let sig = normalize_objection_signature(&long_body);
        assert!(sig.len() <= 200);
    }

    #[test]
    fn normalization_truncates_on_char_boundary_with_multibyte_chars() {
        // A multi-byte char straddling the 200-char cut must not panic
        // (byte-index slicing inside '”' killed the coordinator actor).
        let sig =
            normalize_objection_signature(&format!("{}”{}", "a".repeat(199), "b".repeat(300)));
        assert_eq!(sig.chars().count(), 200);
        assert!(sig.ends_with('”'));
    }

    // ── Shared typed stop contract ───────────────────────────────────────

    #[test]
    fn every_typed_stop_reason_round_trips_structured_context_without_role_fallback() {
        let reasons = vec![
            StopReason::AdversaryDry,
            StopReason::RoundCap,
            StopReason::SpawnCap,
            StopReason::RepeatedObjection {
                signature: "missing acceptance criteria".into(),
                occurrences: 2,
            },
            StopReason::AgentFailure {
                role: RefinementRole::Advocate,
                error_code: "revision_failed".into(),
                message: "advocate could not save the revision".into(),
            },
            StopReason::AgentFailure {
                role: RefinementRole::Adversary,
                error_code: "session_timeout".into(),
                message: "adversary session timed out".into(),
            },
            StopReason::AgentFailure {
                role: RefinementRole::Judge,
                error_code: "verdict_failed".into(),
                message: "judge could not issue a verdict".into(),
            },
            // The convergence aliases remain distinct typed outcomes.
            StopReason::HumanAccepted,
            StopReason::HumanRejected,
            StopReason::Interrupted {
                detail: Some("coordinator restarted".into()),
            },
            StopReason::ReapedPhantom {
                prior_run_id: "run-previous".into(),
                generation: 7,
                evidence_summary: "expired lease with no materialized task".into(),
            },
            StopReason::OperatorStop {
                actor: "operator-42".into(),
                reason: Some("manual cancellation".into()),
            },
            StopReason::UnknownLegacy {
                original_value: "historic_stop".into(),
                source_row: "proposal lifecycle row".into(),
            },
        ];

        for reason in reasons {
            let wire = serde_json::to_value(&reason).expect("typed reason serializes");
            assert_eq!(wire["tag"], reason.tag());
            assert_eq!(
                serde_json::from_value::<StopReason>(wire).expect("typed reason deserializes"),
                reason
            );
        }
    }

    // ── Restored awaiting-review park (startup recovery) ─────────────────

    #[test]
    fn restored_awaiting_review_rebuilds_parked_state() {
        let state = RefinementLoopState::restored_awaiting_review(
            "p1",
            5, // refined revision seq
            2, // snapshot revision seq
            3, // current round
            Some("user-9".to_owned()),
            Some(StopReason::RoundCap),
        );
        // The restored park is indistinguishable from a live converged park:
        // it reports awaiting-review and is neither complete nor mid-loop.
        assert!(state.is_awaiting_human_review());
        assert!(!state.is_complete());
        assert_eq!(state.phase, RefinementPhase::AwaitingHumanReview);
        assert_eq!(state.current_revision_seq, 5);
        assert_eq!(state.snapshot_revision_seq, 2);
        assert_eq!(state.current_round, 3);
        assert_eq!(state.attributed_user_id.as_deref(), Some("user-9"));
        assert_eq!(state.stop_reason, Some(StopReason::RoundCap));
    }

    /// Every ledger-driven rebuild seeds the projection from the proposal's
    /// *current* head, which has already absorbed the tribunal's revisions. The
    /// durable captured seq must win, or the rebuilt projection reports the
    /// refined spec as its own pre-refinement snapshot.
    #[test]
    fn captured_snapshot_seq_overrides_the_head_derived_seed() {
        let state = RefinementLoopState::new("p1", 7).with_captured_snapshot_seq(Some(1));
        assert_eq!(state.snapshot_revision_seq, 1);
        assert_eq!(state.current_revision_seq, 7);

        // A run with no durable start revision keeps the head-derived seed.
        for absent in [None, Some(0), Some(-1)] {
            let state = RefinementLoopState::new("p1", 7).with_captured_snapshot_seq(absent);
            assert_eq!(state.snapshot_revision_seq, 7);
        }
    }

    #[test]
    fn recovered_snapshot_seeds_progress_baseline_from_captured_revision() {
        let state = RefinementLoopState::new("p1", 9).with_recovered_snapshot_seq(Some(2));
        assert_eq!(state.snapshot_revision_seq, 2);
        assert_eq!(state.current_revision_seq, 2);
    }

    #[test]
    fn restored_awaiting_review_resolves_like_a_live_park() {
        let mut state = RefinementLoopState::restored_awaiting_review("p1", 4, 1, 2, None, None);
        // The human accepts: the loop completes exactly as a non-restarted park.
        state.resolve_human_review(true, false);
        assert!(state.is_complete());
        assert_eq!(state.stop_reason, Some(StopReason::HumanAccepted));
    }

    #[test]
    fn restored_awaiting_review_clamps_round_to_at_least_one() {
        let state = RefinementLoopState::restored_awaiting_review("p1", 1, 1, 0, None, None);
        assert_eq!(state.current_round, 1);
    }

    // ── Needs-evidence (AwaitingEvidence) ───────────────────────────────

    #[test]
    fn needs_evidence_parks_loop_in_awaiting_evidence() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        // Advance to judge phase.
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![blocking_objection("Feasibility of X is unclear")],
            explicit_dry: false,
        });
        state.record_advocate_revision(1);
        assert_eq!(state.phase, RefinementPhase::JudgeAdjudication);

        // Judge demands evidence instead of ruling.
        state.record_needs_evidence();
        assert_eq!(state.phase, RefinementPhase::AwaitingEvidence);
        assert!(state.is_awaiting_evidence());
        assert!(!state.is_complete());
        assert!(!state.is_awaiting_human_review());
        // Round counter does NOT advance — the Judge will resume at the same
        // round after the evidence spike produces findings.
        assert_eq!(state.current_round, 1);
        // No stop_reason set — the loop is parked, not terminated.
        assert!(state.stop_reason.is_none());
    }

    #[test]
    fn needs_evidence_from_dry_round() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        // Adversary is dry → straight to judge.
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![],
            explicit_dry: true,
        });
        assert_eq!(state.phase, RefinementPhase::JudgeAdjudication);

        state.record_needs_evidence();
        assert!(state.is_awaiting_evidence());
        assert_eq!(state.current_round, 1);
    }

    #[test]
    fn needs_evidence_does_not_advance_round() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        // Cycle through a full round to round 2.
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![blocking_objection("Issue A")],
            explicit_dry: false,
        });
        state.record_advocate_revision(1);
        state.record_judge_verdict(&JudgeVerdictResult {
            body: "Needs work.".into(),
            blocking: true,
        });
        assert_eq!(state.current_round, 2);

        // Round 2: adversary → advocate → judge demands evidence.
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![blocking_objection("Issue B")],
            explicit_dry: false,
        });
        state.record_advocate_revision(2);
        state.record_needs_evidence();
        assert_eq!(
            state.current_round, 2,
            "round must not advance on needs-evidence"
        );
        assert!(state.is_awaiting_evidence());
    }

    #[test]
    fn evidence_received_resumes_at_advocate_without_advancing_round() {
        let mut state = RefinementLoopState::with_config("p1", 0, test_config());
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![blocking_objection("Need external proof")],
            explicit_dry: false,
        });
        state.record_advocate_revision(1);
        state.record_needs_evidence();
        assert_eq!(state.phase, RefinementPhase::AwaitingEvidence);

        state.resume_after_evidence_received();

        assert_eq!(state.phase, RefinementPhase::AdvocateRevision);
        assert_eq!(state.current_round, 1);
        assert_eq!(state.current_revision_seq, 1);
    }

    // ── Revision event metadata builder ──────────────────────────────────

    #[test]
    fn revision_metadata_contains_expected_fields() {
        let meta = build_revision_event_metadata(2, Some("gpt-4o"));
        assert_eq!(meta["source"], "refinement_loop");
        assert_eq!(meta["role"], "advocate");
        assert_eq!(meta["round"], 2);
        assert_eq!(meta["author_model"], "gpt-4o");
    }

    #[test]
    fn revision_metadata_includes_round_and_role_attribution() {
        for round in 1..=5 {
            let meta =
                build_revision_event_metadata(round, Some("anthropic/claude-sonnet-4-20250514"));
            assert_eq!(meta["round"], round, "round must match for round {round}");
            assert_eq!(meta["role"], "advocate");
            assert_eq!(meta["source"], "refinement_loop");
            assert_eq!(
                meta["author_model"], "anthropic/claude-sonnet-4-20250514",
                "model must be attributed"
            );
        }
    }
}
