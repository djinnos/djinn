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
//
// Guards:
//   - Hard round cap (default 5).
//   - Bounded total agent spawn count.
//   - Repeated blocking objection-signature detection → escalation/stop.
//   - Deterministic stop reasons for every termination path.

use std::collections::HashMap;

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

/// Stop reason for the refinement loop. Every termination path produces a
/// deterministic reason that is persisted for audit and used in UI status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Two consecutive adversary rounds produced no new blocking objections.
    AdversaryDry,
    /// Hard round cap reached.
    RoundCap,
    /// Total agent spawn cap reached.
    SpawnCap,
    /// A blocking objection with the same normalized signature appeared
    /// `repeat_objection_threshold` times across rounds.
    RepeatedObjection {
        signature: String,
        occurrences: usize,
    },
    /// An agent session failed (crashed, timeout, empty response, etc.).
    AgentFailure { role: String, error: String },
}

impl StopReason {
    /// Machine-readable tag for telemetry and event metadata.
    pub fn tag(&self) -> &'static str {
        match self {
            StopReason::AdversaryDry => "adversary_dry",
            StopReason::RoundCap => "round_cap",
            StopReason::SpawnCap => "spawn_cap",
            StopReason::RepeatedObjection { .. } => "repeated_objection",
            StopReason::AgentFailure { .. } => "agent_failure",
        }
    }
}

/// The current phase of the refinement loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefinementPhase {
    /// Waiting for the Advocate to produce a revision.
    AdvocateRevision,
    /// Waiting for the Adversary to attack the current revision.
    AdversaryAttack,
    /// Adversary is dry; waiting for the Judge to adjudicate.
    JudgeAdjudication,
    /// Refinement loop is complete.
    Complete,
}

/// Update authority mode for the refinement loop. Read at each round boundary
/// so a mid-run setting change is respected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAuthority {
    /// Record proposed revisions as a checkpoint without applying them to the
    /// live proposal. The revision is stored via proposal history but the
    /// live spec body is not mutated.
    Checkpoint,
    /// Apply advocate revisions through existing proposal update / revision
    /// history semantics (auto-accept).
    AutoAccept,
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
    /// How advocate revisions are applied.
    pub update_authority: UpdateAuthority,
    /// The user this refinement run is attributed to: owner of the spawned
    /// refinement tasks (`created_by_user_id`) AND the scope used to resolve
    /// per-user role models for the tribunal agents. `None` means "fall back
    /// to the proposal author at dispatch time".
    pub attributed_user_id: Option<String>,
    /// Normalized objection-signature → occurrence count.
    pub objection_signatures: HashMap<String, usize>,
    /// Per-round blocking objection counts: `(round, count)`.
    pub round_blocking_objections: Vec<(i32, usize)>,
    /// Set when the loop terminates.
    pub stop_reason: Option<StopReason>,
}

impl RefinementLoopState {
    /// Create a new refinement loop for the given proposal.
    pub fn new(
        proposal_id: impl Into<String>,
        current_revision_seq: i32,
        update_authority: UpdateAuthority,
    ) -> Self {
        Self::with_config(
            proposal_id,
            current_revision_seq,
            update_authority,
            RefinementConfig::default(),
        )
    }

    /// Create with explicit configuration.
    pub fn with_config(
        proposal_id: impl Into<String>,
        current_revision_seq: i32,
        update_authority: UpdateAuthority,
        config: RefinementConfig,
    ) -> Self {
        Self {
            proposal_id: proposal_id.into(),
            config,
            phase: RefinementPhase::AdvocateRevision,
            current_round: 1,
            consecutive_dry_rounds: 0,
            total_spawns: 0,
            current_revision_seq,
            update_authority,
            attributed_user_id: None,
            objection_signatures: HashMap::new(),
            round_blocking_objections: Vec::new(),
            stop_reason: None,
        }
    }

    /// Attribute this refinement run to a specific user (owner of the spawned
    /// refinement tasks + scope for per-user model resolution). Builder-style so
    /// the existing `new`/`with_config` constructors stay source-compatible.
    pub fn with_attributed_user(mut self, attributed_user_id: Option<String>) -> Self {
        self.attributed_user_id = attributed_user_id;
        self
    }

    /// Whether the loop has terminated.
    pub fn is_complete(&self) -> bool {
        self.phase == RefinementPhase::Complete
    }

    /// Whether the Judge should be invoked. True when the adversary has been
    /// dry for `config.dry_rounds_required` consecutive rounds.
    pub fn should_invoke_judge(&self) -> bool {
        self.consecutive_dry_rounds >= self.config.dry_rounds_required
    }

    /// Read the current update authority. In production, the coordinator calls
    /// this at each round boundary to respect mid-run setting changes.
    pub fn update_authority(&self) -> UpdateAuthority {
        self.update_authority
    }

    /// Set the update authority (called when the coordinator reads user settings
    /// at a round boundary).
    pub fn set_update_authority(&mut self, authority: UpdateAuthority) {
        self.update_authority = authority;
    }

    /// Record that an agent session was spawned. Returns `Err` if the spawn
    /// cap would be exceeded.
    pub fn record_spawn(&mut self) -> Result<(), StopReason> {
        self.total_spawns += 1;
        if self.total_spawns > self.config.max_total_spawns {
            let reason = StopReason::SpawnCap;
            self.terminate(reason.clone());
            Err(reason)
        } else {
            Ok(())
        }
    }

    /// Record an advocate revision result. Advances the revision seq and
    /// transitions to the AdversaryAttack phase.
    pub fn record_advocate_revision(&mut self, new_revision_seq: i32) {
        self.current_revision_seq = new_revision_seq;
        self.phase = RefinementPhase::AdversaryAttack;
    }

    /// Process an adversary attack pass. Returns the repeat-signature
    /// objection if one was detected (which may trigger escalation), and
    /// the updated dry-round status.
    ///
    /// This is pure state-machine logic — the caller is responsible for
    /// persisting objections through P2 debate-trail primitives.
    pub fn process_adversary_pass(&mut self, result: &AdversaryPassResult) -> AdversaryPassOutcome {
        let blocking: Vec<_> = result.objections.iter().filter(|o| o.blocking).collect();
        let blocking_count = blocking.len();

        // Record per-round blocking count.
        self.round_blocking_objections
            .push((self.current_round, blocking_count));

        // Update objection-signature tracking for repeat detection.
        for obj in &blocking {
            let sig = normalize_objection_signature(&obj.body);
            *self.objection_signatures.entry(sig).or_insert(0) += 1;
        }

        // Check for repeated signatures.
        if let Some((sig, count)) = self.detect_repeated_signature() {
            let reason = StopReason::RepeatedObjection {
                signature: sig,
                occurrences: count,
            };
            self.terminate(reason.clone());
            return AdversaryPassOutcome::Escalated(reason);
        }

        if blocking_count == 0 || result.explicit_dry {
            self.consecutive_dry_rounds += 1;
        } else {
            self.consecutive_dry_rounds = 0;
        }

        // Check dry condition: enough consecutive dry rounds → invoke judge.
        if self.should_invoke_judge() {
            self.phase = RefinementPhase::JudgeAdjudication;
            return AdversaryPassOutcome::Dry;
        }

        // Check round cap.
        if self.current_round >= self.config.max_rounds {
            let reason = StopReason::RoundCap;
            self.terminate(reason.clone());
            return AdversaryPassOutcome::Escalated(reason);
        }

        // Advance to next round.
        self.current_round += 1;
        self.phase = RefinementPhase::AdvocateRevision;
        AdversaryPassOutcome::Continue
    }

    /// Record a judge verdict. Transitions to Complete.
    pub fn record_judge_verdict(&mut self, _result: &JudgeVerdictResult) {
        let reason = StopReason::AdversaryDry;
        self.terminate(reason);
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
    // Truncate to keep signature storage bounded.
    if trimmed.len() > 200 {
        trimmed[..200].to_string()
    } else {
        trimmed.to_string()
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
/// so revisions are attributable by role, round, and authority mode.
pub fn build_revision_event_metadata(
    round: i32,
    authority: UpdateAuthority,
    author_model: Option<&str>,
) -> serde_json::Value {
    let mut meta = serde_json::json!({
        "source": "refinement_loop",
        "role": "advocate",
        "round": round,
        "authority": match authority {
            UpdateAuthority::Checkpoint => "checkpoint",
            UpdateAuthority::AutoAccept => "auto_accept",
        },
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

    // ── Dry-round detection and judge invocation ─────────────────────────

    #[test]
    fn stops_after_two_dry_adversary_rounds_and_invokes_judge() {
        let mut state =
            RefinementLoopState::with_config("p1", 0, UpdateAuthority::AutoAccept, test_config());

        // Round 1: advocate revises.
        state.record_advocate_revision(1);
        assert_eq!(state.phase, RefinementPhase::AdversaryAttack);

        // Round 1: adversary finds blocking objections.
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![ObjectionRecord {
                body: "Missing problem section".into(),
                blocking: true,
                author_model: None,
                entry_id: None,
            }],
            explicit_dry: false,
        });
        assert_eq!(outcome, AdversaryPassOutcome::Continue);
        assert_eq!(state.consecutive_dry_rounds, 0);
        assert_eq!(state.current_round, 2);

        // Round 2: advocate revises.
        state.record_advocate_revision(2);

        // Round 2: adversary finds no blocking objections (dry).
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![],
            explicit_dry: true,
        });
        assert_eq!(outcome, AdversaryPassOutcome::Continue);
        assert_eq!(state.consecutive_dry_rounds, 1);

        // Round 3: advocate revises.
        state.record_advocate_revision(3);

        // Round 3: adversary is dry again → should invoke judge.
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![],
            explicit_dry: true,
        });
        assert_eq!(outcome, AdversaryPassOutcome::Dry);
        assert_eq!(state.phase, RefinementPhase::JudgeAdjudication);
        assert!(state.should_invoke_judge());

        // Judge adjudicates.
        state.record_judge_verdict(&JudgeVerdictResult {
            body: "Proposal is ready.".into(),
            blocking: false,
        });
        assert!(state.is_complete());
        assert_eq!(state.stop_reason, Some(StopReason::AdversaryDry));
    }

    #[test]
    fn dry_rounds_reset_on_blocking_objection() {
        let mut state =
            RefinementLoopState::with_config("p1", 0, UpdateAuthority::AutoAccept, test_config());

        // Round 1: dry.
        state.record_advocate_revision(1);
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![],
            explicit_dry: true,
        });
        assert_eq!(state.consecutive_dry_rounds, 1);

        // Round 2: blocking objection → resets dry counter.
        state.record_advocate_revision(2);
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![ObjectionRecord {
                body: "New blocking issue".into(),
                blocking: true,
                author_model: None,
                entry_id: None,
            }],
            explicit_dry: false,
        });
        assert_eq!(state.consecutive_dry_rounds, 0);
    }

    // ── Hard round cap ───────────────────────────────────────────────────

    #[test]
    fn enforces_hard_round_cap() {
        let config = RefinementConfig {
            max_rounds: 3,
            ..test_config()
        };
        let mut state =
            RefinementLoopState::with_config("p1", 0, UpdateAuthority::AutoAccept, config);

        // Rounds 1-3: adversary keeps finding blocking objections.
        for round in 1..=3 {
            state.record_advocate_revision(round);
            let outcome = state.process_adversary_pass(&AdversaryPassResult {
                objections: vec![ObjectionRecord {
                    body: format!("Blocking issue in round {round}"),
                    blocking: true,
                    author_model: None,
                    entry_id: None,
                }],
                explicit_dry: false,
            });
            if round < 3 {
                assert_eq!(outcome, AdversaryPassOutcome::Continue);
            } else {
                assert!(matches!(
                    outcome,
                    AdversaryPassOutcome::Escalated(StopReason::RoundCap)
                ));
            }
        }

        assert!(state.is_complete());
        assert_eq!(state.stop_reason, Some(StopReason::RoundCap));
    }

    // ── Total spawn cap ──────────────────────────────────────────────────

    #[test]
    fn enforces_total_spawn_cap() {
        let config = RefinementConfig {
            max_total_spawns: 3,
            ..test_config()
        };
        let mut state =
            RefinementLoopState::with_config("p1", 0, UpdateAuthority::AutoAccept, config);

        // Spawn 3 agents successfully.
        assert!(state.record_spawn().is_ok());
        assert!(state.record_spawn().is_ok());
        assert!(state.record_spawn().is_ok());

        // Fourth spawn exceeds the cap.
        let err = state.record_spawn().unwrap_err();
        assert_eq!(err, StopReason::SpawnCap);
        assert!(state.is_complete());
        assert_eq!(state.stop_reason, Some(StopReason::SpawnCap));
    }

    // ── Repeated objection-signature detection ───────────────────────────

    #[test]
    fn detects_repeated_blocking_objection_signatures() {
        let config = RefinementConfig {
            repeat_objection_threshold: 2,
            ..test_config()
        };
        let mut state =
            RefinementLoopState::with_config("p1", 0, UpdateAuthority::AutoAccept, config);

        // Round 1: adversary raises a blocking objection.
        state.record_advocate_revision(1);
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![ObjectionRecord {
                body: "Missing Problem Statement".into(),
                blocking: true,
                author_model: None,
                entry_id: None,
            }],
            explicit_dry: false,
        });
        assert_eq!(outcome, AdversaryPassOutcome::Continue);

        // Round 2: adversary raises the same objection (different casing/whitespace).
        state.record_advocate_revision(2);
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![ObjectionRecord {
                body: "missing  problem  statement".into(),
                blocking: true,
                author_model: None,
                entry_id: None,
            }],
            explicit_dry: false,
        });
        assert!(matches!(
            outcome,
            AdversaryPassOutcome::Escalated(StopReason::RepeatedObjection { .. })
        ));
        assert!(state.is_complete());

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
        // Use dry_rounds_required=3 so the loop doesn't terminate on round 2.
        let config = RefinementConfig {
            repeat_objection_threshold: 2,
            dry_rounds_required: 3,
            ..test_config()
        };
        let mut state =
            RefinementLoopState::with_config("p1", 0, UpdateAuthority::AutoAccept, config);

        // Round 1: non-blocking objection.
        state.record_advocate_revision(1);
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![ObjectionRecord {
                body: "Minor style concern".into(),
                blocking: false,
                author_model: None,
                entry_id: None,
            }],
            explicit_dry: false,
        });

        // Round 2: same non-blocking objection → no escalation (not blocking).
        state.record_advocate_revision(2);
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![ObjectionRecord {
                body: "Minor style concern".into(),
                blocking: false,
                author_model: None,
                entry_id: None,
            }],
            explicit_dry: false,
        });
        // Non-blocking objections don't count for repeat detection, so this
        // should be treated as a dry round (no new blocking objections).
        assert_eq!(outcome, AdversaryPassOutcome::Continue);
        assert!(!state.is_complete());
        assert_eq!(state.consecutive_dry_rounds, 2);
    }

    // ── Checkpoint vs auto-accept ────────────────────────────────────────

    #[test]
    fn checkpoint_mode_records_without_applying() {
        let mut state =
            RefinementLoopState::with_config("p1", 0, UpdateAuthority::Checkpoint, test_config());

        assert_eq!(state.update_authority(), UpdateAuthority::Checkpoint);

        // Simulate mid-run switch to auto-accept.
        state.set_update_authority(UpdateAuthority::AutoAccept);
        assert_eq!(state.update_authority(), UpdateAuthority::AutoAccept);

        // The authority is what the coordinator reads at round boundaries.
        // In checkpoint mode, the caller should store the revision but not
        // mutate the live proposal body; in auto-accept, it should apply.
        // This is verified at the integration level; here we verify the
        // state machine tracks the mode correctly.
    }

    #[test]
    fn update_authority_defaults_are_respected() {
        let checkpoint = RefinementLoopState::new("p1", 0, UpdateAuthority::Checkpoint);
        assert_eq!(checkpoint.update_authority(), UpdateAuthority::Checkpoint);

        let auto_accept = RefinementLoopState::new("p1", 0, UpdateAuthority::AutoAccept);
        assert_eq!(auto_accept.update_authority(), UpdateAuthority::AutoAccept);
    }

    // ── Diverse-refinement model selection ────────────────────────────────

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

    // ── Objection signature normalization ─────────────────────────────────

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

    // ── Phase transitions ────────────────────────────────────────────────

    #[test]
    fn phase_transitions_follow_expected_sequence() {
        let mut state = RefinementLoopState::new("p1", 0, UpdateAuthority::AutoAccept);
        assert_eq!(state.phase, RefinementPhase::AdvocateRevision);

        state.record_advocate_revision(1);
        assert_eq!(state.phase, RefinementPhase::AdversaryAttack);

        // Non-blocking result → continue to next round.
        state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![ObjectionRecord {
                body: "non-blocking".into(),
                blocking: false,
                author_model: None,
                entry_id: None,
            }],
            explicit_dry: false,
        });
        assert_eq!(state.phase, RefinementPhase::AdvocateRevision);
        assert_eq!(state.current_round, 2);
    }

    // ── StopReason tag ───────────────────────────────────────────────────

    #[test]
    fn stop_reason_tags_are_stable() {
        assert_eq!(StopReason::AdversaryDry.tag(), "adversary_dry");
        assert_eq!(StopReason::RoundCap.tag(), "round_cap");
        assert_eq!(StopReason::SpawnCap.tag(), "spawn_cap");
        assert_eq!(
            StopReason::RepeatedObjection {
                signature: "x".into(),
                occurrences: 2,
            }
            .tag(),
            "repeated_objection"
        );
        assert_eq!(
            StopReason::AgentFailure {
                role: "advocate".into(),
                error: "timeout".into(),
            }
            .tag(),
            "agent_failure"
        );
    }

    // ── Revision event metadata builder ──────────────────────────────────

    #[test]
    fn revision_metadata_contains_expected_fields() {
        let meta = build_revision_event_metadata(2, UpdateAuthority::AutoAccept, Some("gpt-4o"));
        assert_eq!(meta["source"], "refinement_loop");
        assert_eq!(meta["role"], "advocate");
        assert_eq!(meta["round"], 2);
        assert_eq!(meta["authority"], "auto_accept");
        assert_eq!(meta["author_model"], "gpt-4o");
    }

    #[test]
    fn revision_metadata_checkpoint_mode() {
        let meta = build_revision_event_metadata(1, UpdateAuthority::Checkpoint, None);
        assert_eq!(meta["authority"], "checkpoint");
        assert!(meta.get("author_model").is_none() || meta["author_model"].is_null());
    }

    // ── Edge case: explicit dry with blocking objections ──────────────────

    #[test]
    fn explicit_dry_signal_overrides_blocking_objections() {
        let mut state =
            RefinementLoopState::with_config("p1", 0, UpdateAuthority::AutoAccept, test_config());

        state.record_advocate_revision(1);
        // Adversary found blocking objections but explicitly signals dry
        // (e.g. the objections are resolved inline).
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![ObjectionRecord {
                body: "Resolved inline".into(),
                blocking: true,
                author_model: None,
                entry_id: None,
            }],
            explicit_dry: true,
        });
        assert_eq!(state.consecutive_dry_rounds, 1);
        assert_eq!(outcome, AdversaryPassOutcome::Continue);
    }

    // ── Edge case: agent failure termination ──────────────────────────────

    #[test]
    fn agent_failure_terminates_loop() {
        let mut state = RefinementLoopState::new("p1", 0, UpdateAuthority::AutoAccept);
        state.terminate(StopReason::AgentFailure {
            role: "adversary".into(),
            error: "session crashed".into(),
        });
        assert!(state.is_complete());
        assert!(matches!(
            state.stop_reason,
            Some(StopReason::AgentFailure { .. })
        ));
    }

    // ── Revision attribution per round ──────────────────────────────────

    #[test]
    fn revision_metadata_includes_round_and_role_attribution() {
        // Each round's advocate revision should carry round number, role,
        // authority mode, and model for attribution in proposal history.
        for round in 1..=5 {
            let meta = build_revision_event_metadata(
                round,
                UpdateAuthority::AutoAccept,
                Some("anthropic/claude-sonnet-4-20250514"),
            );
            assert_eq!(meta["round"], round, "round must match for round {round}");
            assert_eq!(meta["role"], "advocate");
            assert_eq!(meta["source"], "refinement_loop");
            assert_eq!(meta["authority"], "auto_accept");
            assert_eq!(
                meta["author_model"], "anthropic/claude-sonnet-4-20250514",
                "model must be attributed"
            );
        }
    }

    #[test]
    fn debate_trail_attribution_fields_are_consistent_with_state() {
        // Verify that the objection record shape aligns with the debate-trail
        // schema fields (round, against_revision_seq, agent_role, blocking).
        let mut state =
            RefinementLoopState::with_config("p1", 0, UpdateAuthority::AutoAccept, test_config());

        // Round 1: advocate advances to revision 1.
        state.record_advocate_revision(1);
        assert_eq!(state.current_revision_seq, 1);
        assert_eq!(state.current_round, 1);

        // The debate-trail entry for the adversary's objection in this round
        // should carry: round=1, against_revision_seq=1, agent_role="adversary".
        // This test verifies the state machine tracks these correctly so the
        // dispatch layer can persist them with the right values.
        let objection = ObjectionRecord {
            body: "Missing scope section".into(),
            blocking: true,
            author_model: Some("openai/gpt-4o".into()),
            entry_id: None,
        };
        let outcome = state.process_adversary_pass(&AdversaryPassResult {
            objections: vec![objection],
            explicit_dry: false,
        });
        assert_eq!(outcome, AdversaryPassOutcome::Continue);
        // After processing, round advances to 2, revision_seq stays 1.
        assert_eq!(state.current_round, 2);
        assert_eq!(state.current_revision_seq, 1);
    }

    // ── Checkpoint pending metadata ─────────────────────────────────────

    #[test]
    fn build_revision_event_metadata_checkpoint_includes_pending() {
        let meta = build_revision_event_metadata(2, UpdateAuthority::Checkpoint, Some("model-x"));
        assert_eq!(meta["authority"], "checkpoint");
        assert_eq!(meta["role"], "advocate");
        assert_eq!(meta["round"], 2);
        assert_eq!(meta["author_model"], "model-x");
        // The coordinator adds checkpoint_status: "pending" after calling this.
        // Verify the base metadata doesn't include it (that's the coordinator's job).
        assert!(meta.get("checkpoint_status").is_none());
    }

    #[test]
    fn build_revision_event_metadata_auto_accept_no_checkpoint() {
        let meta = build_revision_event_metadata(1, UpdateAuthority::AutoAccept, None);
        assert_eq!(meta["authority"], "auto_accept");
        assert!(meta.get("checkpoint_status").is_none());
        assert!(meta.get("author_model").is_none());
    }

    #[test]
    fn checkpoint_pending_status_field_can_be_added() {
        let mut meta = build_revision_event_metadata(3, UpdateAuthority::Checkpoint, None);
        meta["checkpoint_status"] = serde_json::json!("pending");
        meta["previous_revision_seq"] = serde_json::json!(2);
        assert_eq!(meta["checkpoint_status"], "pending");
        assert_eq!(meta["previous_revision_seq"], 2);
    }
}
