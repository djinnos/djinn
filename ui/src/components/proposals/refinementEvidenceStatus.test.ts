import { describe, expect, it } from "vitest";
import type {
  ProposalRefinementStatus,
  NeedsEvidenceStatus,
} from "@/api/types";
import { classifyRefinementEvidence } from "./refinementEvidenceStatus";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/** Minimal active in-progress refinement with no evidence lifecycle. */
const activeStatus: ProposalRefinementStatus = {
  active: true,
  current_round: 2,
  dry_rounds: 0,
  total_entries: 5,
  stop_reason: null,
  awaiting_review: false,
};

/** Awaiting-evidence lifecycle on the status object. */
const awaitingEvidenceStatus: ProposalRefinementStatus = {
  ...activeStatus,
  evidence_lifecycle_state: "awaiting_evidence",
  needs_evidence: {
    claim: "Can the Rust proc-macro crate emit JSON?",
    spike_task_id: "11111111-1111-1111-1111-111111111111",
    spike_short_id: "sp-1",
    spike_status: "in_progress",
    evidence_phase: "awaiting_evidence",
    question: "Can the Rust proc-macro crate emit JSON?",
    round: 2,
  },
};

/** Evidence-failed lifecycle on the status object. */
const evidenceFailedStatus: ProposalRefinementStatus = {
  ...activeStatus,
  evidence_lifecycle_state: "evidence_failed",
  needs_evidence: {
    claim: "Does the API support webhooks?",
    spike_task_id: "22222222-2222-2222-2222-222222222222",
    spike_short_id: "sp-2",
    spike_status: "cancelled",
    evidence_phase: "evidence_failed",
    failure_reason: "spike_cancelled",
    round: 1,
  },
};

/** Paused-or-frozen lifecycle. */
const pausedStatus: ProposalRefinementStatus = {
  ...activeStatus,
  active: false,
  evidence_lifecycle_state: "paused_or_frozen",
  needs_evidence: {
    claim: "Latency under 100ms?",
    spike_task_id: "33333333-3333-3333-3333-333333333333",
    spike_short_id: "sp-3",
    spike_status: "in_progress",
    evidence_phase: "awaiting_evidence",
  },
};

/** Awaiting-review (converged) refinement. */
const awaitingReviewStatus: ProposalRefinementStatus = {
  ...activeStatus,
  active: false,
  awaiting_review: true,
  stop_reason: null,
};

/** Refinement stopped with a stop_reason. */
const stoppedStatus: ProposalRefinementStatus = {
  active: false,
  dry_rounds: 3,
  total_entries: 12,
  stop_reason: "adversary_dry",
  awaiting_review: false,
};

/** Evidence-received lifecycle (transitional). */
const evidenceReceivedStatus: ProposalRefinementStatus = {
  ...activeStatus,
  evidence_lifecycle_state: "evidence_received",
  needs_evidence: {
    claim: "Scalability test",
    spike_task_id: "44444444-4444-4444-4444-444444444444",
    spike_short_id: "sp-4",
    spike_status: "done",
    evidence_phase: "evidence_received",
  },
};

/** Gate-level evidence for fallback tests. */
const gateEvidence: NeedsEvidenceStatus = {
  claim: "Fallback claim from gate",
  spike_task_id: "55555555-5555-5555-5555-555555555555",
  spike_short_id: "sp-gate",
  spike_status: "in_progress",
  evidence_phase: "awaiting_evidence",
  question: "Can this work?",
  round: 3,
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("classifyRefinementEvidence", () => {
  // ---- Not started / null ------------------------------------------------

  it("returns not_started when status is null", () => {
    const d = classifyRefinementEvidence(null);
    expect(d.kind).toBe("not_started");
    expect(d.badge).toBe("");
    expect(d.showInProgress).toBe(false);
    expect(d.suppressAutoResume).toBe(false);
    expect(d.evidence).toBeNull();
    expect(d.spikeShortId).toBeNull();
    expect(d.spikeTaskId).toBeNull();
    expect(d.claimSummary).toBeNull();
    expect(d.failureReason).toBeNull();
    expect(d.isPausedFrozen).toBe(false);
  });

  // ---- Ordinary in-progress -----------------------------------------------

  it("classifies active non-evidence refinement as in_progress", () => {
    const d = classifyRefinementEvidence(activeStatus);
    expect(d.kind).toBe("in_progress");
    expect(d.badge).toBe("In progress");
    expect(d.showInProgress).toBe(true);
    expect(d.suppressAutoResume).toBe(false);
    expect(d.evidence).toBeNull();
    expect(d.isPausedFrozen).toBe(false);
  });

  // ---- Awaiting evidence --------------------------------------------------

  it("classifies lifecycle awaiting_evidence", () => {
    const d = classifyRefinementEvidence(awaitingEvidenceStatus);
    expect(d.kind).toBe("awaiting_evidence");
    expect(d.badge).toBe("Awaiting evidence (sp-1)");
    expect(d.showInProgress).toBe(false);
    expect(d.suppressAutoResume).toBe(true);
    expect(d.evidence).toBe(awaitingEvidenceStatus.needs_evidence);
    expect(d.spikeShortId).toBe("sp-1");
    expect(d.spikeTaskId).toBe("11111111-1111-1111-1111-111111111111");
    expect(d.claimSummary).toBe("Can the Rust proc-macro crate emit JSON?");
    expect(d.failureReason).toBeNull();
    expect(d.isPausedFrozen).toBe(false);
  });

  it("classifies awaiting from needs_evidence evidence_phase when lifecycle is missing", () => {
    const status: ProposalRefinementStatus = {
      ...activeStatus,
      // no evidence_lifecycle_state
      needs_evidence: {
        claim: "Test claim",
        spike_task_id: "aaa",
        spike_short_id: "sp-x",
        spike_status: "in_progress",
        evidence_phase: "awaiting_evidence",
      },
    };
    const d = classifyRefinementEvidence(status);
    expect(d.kind).toBe("awaiting_evidence");
    expect(d.spikeShortId).toBe("sp-x");
  });

  it("classifies implicit awaiting when spike is open and no phase/failure set", () => {
    const status: ProposalRefinementStatus = {
      ...activeStatus,
      needs_evidence: {
        claim: "Implicit open",
        spike_task_id: "bbb",
        spike_short_id: "sp-imp",
        spike_status: "in_progress",
        // no evidence_phase, no failure_reason
      },
    };
    const d = classifyRefinementEvidence(status);
    expect(d.kind).toBe("awaiting_evidence");
    expect(d.spikeShortId).toBe("sp-imp");
  });

  it("badge has no spike id when needs_evidence lacks spike_short_id", () => {
    const status: ProposalRefinementStatus = {
      ...activeStatus,
      evidence_lifecycle_state: "awaiting_evidence",
      needs_evidence: {
        claim: "No short id",
        spike_task_id: "ccc",
        spike_short_id: "",
        spike_status: "in_progress",
      },
    };
    const d = classifyRefinementEvidence(status);
    expect(d.kind).toBe("awaiting_evidence");
    expect(d.badge).toBe("Awaiting evidence");
  });

  // ---- Evidence failed ----------------------------------------------------

  it("classifies lifecycle evidence_failed", () => {
    const d = classifyRefinementEvidence(evidenceFailedStatus);
    expect(d.kind).toBe("evidence_failed");
    expect(d.badge).toBe("Evidence failed");
    expect(d.showInProgress).toBe(false);
    expect(d.suppressAutoResume).toBe(true);
    expect(d.failureReason).toBe("spike_cancelled");
    expect(d.spikeShortId).toBe("sp-2");
    expect(d.claimSummary).toBe("Does the API support webhooks?");
    expect(d.isPausedFrozen).toBe(false);
  });

  it("classifies failed from needs_evidence failure_reason when lifecycle is missing", () => {
    const status: ProposalRefinementStatus = {
      ...activeStatus,
      needs_evidence: {
        claim: "Test",
        spike_task_id: "ddd",
        spike_short_id: "sp-f",
        spike_status: "error",
        failure_reason: "spike_errored",
      },
    };
    const d = classifyRefinementEvidence(status);
    expect(d.kind).toBe("evidence_failed");
    expect(d.failureReason).toBe("spike_errored");
  });

  it("classifies failed from terminal spike_status (cancelled) when no phase set", () => {
    const status: ProposalRefinementStatus = {
      ...activeStatus,
      needs_evidence: {
        claim: "Terminal",
        spike_task_id: "eee",
        spike_short_id: "sp-t",
        spike_status: "cancelled",
        // no evidence_phase, no failure_reason
      },
    };
    const d = classifyRefinementEvidence(status);
    expect(d.kind).toBe("evidence_failed");
  });

  it("does NOT classify spike_status=cancelled as failed when evidence_phase is received", () => {
    const status: ProposalRefinementStatus = {
      ...activeStatus,
      needs_evidence: {
        claim: "Already received",
        spike_task_id: "fff",
        spike_short_id: "sp-r",
        spike_status: "cancelled",
        evidence_phase: "evidence_received",
      },
    };
    const d = classifyRefinementEvidence(status);
    // evidence_received lifecycle check fires before the spike_status fallback
    expect(d.kind).toBe("evidence_received");
  });

  // ---- Paused / frozen (highest precedence) --------------------------------

  it("classifies paused_or_frozen and suppresses auto-resume", () => {
    const d = classifyRefinementEvidence(pausedStatus);
    expect(d.kind).toBe("paused_frozen");
    expect(d.badge).toBe("Paused");
    expect(d.showInProgress).toBe(false);
    expect(d.suppressAutoResume).toBe(true);
    expect(d.isPausedFrozen).toBe(true);
    // evidence context still carried for the renderer
    expect(d.evidence).toBe(pausedStatus.needs_evidence);
    expect(d.spikeShortId).toBe("sp-3");
    expect(d.claimSummary).toBe("Latency under 100ms?");
  });

  it("paused_frozen takes precedence even when needs_evidence indicates awaiting", () => {
    const status: ProposalRefinementStatus = {
      ...activeStatus,
      active: false,
      evidence_lifecycle_state: "paused_or_frozen",
      needs_evidence: {
        claim: "Should be awaiting",
        spike_task_id: "ggg",
        spike_short_id: "sp-p",
        spike_status: "in_progress",
        evidence_phase: "awaiting_evidence",
      },
    };
    const d = classifyRefinementEvidence(status);
    expect(d.kind).toBe("paused_frozen");
    expect(d.suppressAutoResume).toBe(true);
  });

  it("paused_frozen takes precedence even when needs_evidence indicates failed", () => {
    const status: ProposalRefinementStatus = {
      ...activeStatus,
      active: false,
      evidence_lifecycle_state: "paused_or_frozen",
      needs_evidence: {
        claim: "Should be failed",
        spike_task_id: "hhh",
        spike_short_id: "sp-pf",
        spike_status: "error",
        evidence_phase: "evidence_failed",
        failure_reason: "spike_errored",
      },
    };
    const d = classifyRefinementEvidence(status);
    expect(d.kind).toBe("paused_frozen");
    expect(d.suppressAutoResume).toBe(true);
  });

  // ---- Awaiting review / converged (not reclassified) ----------------------

  it("classifies awaiting_review and does not reclassify as evidence status", () => {
    const d = classifyRefinementEvidence(awaitingReviewStatus);
    expect(d.kind).toBe("awaiting_review");
    expect(d.badge).toBe("Awaiting review");
    expect(d.showInProgress).toBe(false);
    expect(d.isPausedFrozen).toBe(false);
    expect(d.spikeShortId).toBeNull();
    expect(d.failureReason).toBeNull();
  });

  it("awaiting_review is not reclassified even when needs_evidence is present", () => {
    const status: ProposalRefinementStatus = {
      ...awaitingReviewStatus,
      evidence_lifecycle_state: "awaiting_evidence",
      needs_evidence: {
        claim: "Should not matter",
        spike_task_id: "iii",
        spike_short_id: "sp-ar",
        spike_status: "in_progress",
        evidence_phase: "awaiting_evidence",
      },
    };
    const d = classifyRefinementEvidence(status);
    expect(d.kind).toBe("awaiting_review");
  });

  // ---- Evidence received (transitional) ------------------------------------

  it("classifies evidence_received and allows in-progress copy", () => {
    const d = classifyRefinementEvidence(evidenceReceivedStatus);
    expect(d.kind).toBe("evidence_received");
    expect(d.badge).toBe("Evidence received");
    expect(d.showInProgress).toBe(true);
    expect(d.suppressAutoResume).toBe(false);
    expect(d.evidence).toBe(evidenceReceivedStatus.needs_evidence);
    expect(d.spikeShortId).toBe("sp-4");
  });

  // ---- Terminal / stopped --------------------------------------------------

  it("classifies terminal lifecycle state", () => {
    const status: ProposalRefinementStatus = {
      ...stoppedStatus,
      evidence_lifecycle_state: "terminal",
    };
    const d = classifyRefinementEvidence(status);
    expect(d.kind).toBe("terminal");
    expect(d.badge).toBe("Stopped (adversary dry)");
    expect(d.showInProgress).toBe(false);
  });

  it("classifies stopped by stop_reason when lifecycle is absent", () => {
    const d = classifyRefinementEvidence(stoppedStatus);
    expect(d.kind).toBe("terminal");
    expect(d.badge).toBe("Stopped (adversary dry)");
  });

  it("uses fallback label for unknown stop_reason", () => {
    const status: ProposalRefinementStatus = {
      ...stoppedStatus,
      stop_reason: "custom_reason",
    };
    const d = classifyRefinementEvidence(status);
    expect(d.kind).toBe("terminal");
    expect(d.badge).toBe("Stopped (custom_reason)");
  });

  // ---- Gate evidence fallback ----------------------------------------------

  it("falls back to gate-level needs_evidence when status.needs_evidence is absent", () => {
    const status: ProposalRefinementStatus = {
      ...activeStatus,
      evidence_lifecycle_state: "awaiting_evidence",
      // needs_evidence is absent
    };
    const d = classifyRefinementEvidence(status, gateEvidence);
    expect(d.kind).toBe("awaiting_evidence");
    expect(d.evidence).toBe(gateEvidence);
    expect(d.spikeShortId).toBe("sp-gate");
    expect(d.claimSummary).toBe("Can this work?");
  });

  it("prefers status.needs_evidence over gate-level evidence", () => {
    const d = classifyRefinementEvidence(awaitingEvidenceStatus, gateEvidence);
    expect(d.evidence).toBe(awaitingEvidenceStatus.needs_evidence);
    expect(d.spikeShortId).toBe("sp-1");
  });

  it("gate fallback provides evidence for implicit awaiting when status has no needs_evidence", () => {
    const status: ProposalRefinementStatus = {
      ...activeStatus,
      // no evidence_lifecycle_state, no needs_evidence
    };
    const d = classifyRefinementEvidence(status, gateEvidence);
    expect(d.kind).toBe("awaiting_evidence");
    expect(d.evidence).toBe(gateEvidence);
  });

  it("gate fallback does not override null status (stays not_started)", () => {
    const d = classifyRefinementEvidence(null, gateEvidence);
    expect(d.kind).toBe("not_started");
    expect(d.evidence).toBeNull();
  });

  // ---- Question vs claim precedence ----------------------------------------

  it("prefers question over claim for claimSummary when available", () => {
    const d = classifyRefinementEvidence(awaitingEvidenceStatus);
    // awaitingEvidenceStatus has question === claim, so check explicit precedence
    expect(d.claimSummary).toBe(
      awaitingEvidenceStatus.needs_evidence?.question,
    );
  });

  it("falls back to claim when question is absent", () => {
    const status: ProposalRefinementStatus = {
      ...activeStatus,
      evidence_lifecycle_state: "awaiting_evidence",
      needs_evidence: {
        claim: "Plain claim without question",
        spike_task_id: "jjj",
        spike_short_id: "sp-nq",
        spike_status: "in_progress",
        // no question
      },
    };
    const d = classifyRefinementEvidence(status);
    expect(d.claimSummary).toBe("Plain claim without question");
  });
});
