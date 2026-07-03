import type {
  ProposalRefinementStatus,
  NeedsEvidenceStatus,
} from "@/api/types";

/**
 * Display classification for a proposal's refinement evidence state.
 *
 * Priority order (highest first):
 * 1. `paused_frozen` — manual pause/freeze suppresses auto-resume
 * 2. `awaiting_review` — converged tribunal; never reclassified
 * 3. `evidence_failed` — blocked on missing/failed findings
 * 4. `awaiting_evidence` — evidence spike is open
 * 5. `evidence_received` — evidence came back; refinement may resume
 * 6. `terminal` — refinement stopped (stop_reason or terminal lifecycle)
 * 7. `in_progress` — ordinary active refinement
 * 8. `not_started` — no refinement yet / null status
 */
export type RefinementEvidenceDisplayKind =
  | "paused_frozen"
  | "awaiting_review"
  | "evidence_failed"
  | "awaiting_evidence"
  | "evidence_received"
  | "terminal"
  | "in_progress"
  | "not_started";

/**
 * Display-safe data derived from a proposal's refinement status and optional
 * gate-level evidence.  Renderers use this instead of branching on raw status
 * fields, so classification logic lives in one place.
 */
export interface RefinementEvidenceDisplay {
  /** The classified display kind. */
  kind: RefinementEvidenceDisplayKind;
  /** Badge / ribbon label for the refinement status. */
  badge: string;
  /** Whether the ordinary autonomous in-progress copy should render. */
  showInProgress: boolean;
  /** Whether automatic-running / automatic-resume wording must be suppressed. */
  suppressAutoResume: boolean;
  /**
   * The selected needs-evidence object.
   * Prefers `status.needs_evidence`; falls back to the gate-level evidence
   * when the status object lacks it.
   */
  evidence: NeedsEvidenceStatus | null;
  /** Spike short id for display, or `null` when no evidence. */
  spikeShortId: string | null;
  /** Spike task id for display, or `null` when no evidence. */
  spikeTaskId: string | null;
  /** Claim or question summary, or `null` when no evidence. */
  claimSummary: string | null;
  /** Failure reason from evidence (non-null only when kind is `evidence_failed`). */
  failureReason: string | null;
  /** Whether paused/frozen precedence was applied. */
  isPausedFrozen: boolean;
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/**
 * Choose the best-available needs-evidence object.
 * Status-level takes priority; gate-level is the fallback for components
 * whose props already carry gate evidence.
 */
function resolveEvidence(
  status: ProposalRefinementStatus | null,
  gateEvidence?: NeedsEvidenceStatus | null,
): NeedsEvidenceStatus | null {
  return status?.needs_evidence ?? gateEvidence ?? null;
}

/** Returns `true` when the evidence indicates an open / awaiting state. */
function isAwaitingEvidence(
  lifecycle: string | undefined,
  evidence: NeedsEvidenceStatus | null,
): boolean {
  if (lifecycle === "awaiting_evidence") return true;
  if (!evidence) return false;
  if (evidence.evidence_phase === "awaiting_evidence") return true;
  // An open spike without a phase or failure is implicitly awaiting.
  if (
    !evidence.evidence_phase &&
    !evidence.failure_reason &&
    evidence.spike_status !== "cancelled" &&
    evidence.spike_status !== "error" &&
    evidence.spike_status !== "force_closed"
  ) {
    return true;
  }
  return false;
}

/** Returns `true` when the evidence indicates a failed state. */
function isEvidenceFailed(
  lifecycle: string | undefined,
  evidence: NeedsEvidenceStatus | null,
): boolean {
  if (lifecycle === "evidence_failed") return true;
  if (!evidence) return false;
  if (evidence.evidence_phase === "failed") return true;
  if (evidence.failure_reason) return true;
  // Terminal spike statuses without received evidence count as failed.
  if (
    evidence.evidence_phase !== "evidence_received" &&
    (evidence.spike_status === "cancelled" ||
      evidence.spike_status === "error" ||
      evidence.spike_status === "force_closed")
  ) {
    return true;
  }
  return false;
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/**
 * Classify a proposal's refinement evidence state into display-safe data.
 *
 * @param status  The proposal's refinement status (may be `null`).
 * @param gateEvidence  Optional gate-level `needs_evidence` from
 *   `ProposalGateStatus`, used as fallback when `status.needs_evidence` is
 *   absent.
 */
export function classifyRefinementEvidence(
  status: ProposalRefinementStatus | null,
  gateEvidence?: NeedsEvidenceStatus | null,
): RefinementEvidenceDisplay {
  // When status is null, refinement has never started. Gate evidence is
  // irrelevant without a status object to anchor it.
  if (!status) {
    return {
      kind: "not_started",
      badge: "",
      showInProgress: false,
      suppressAutoResume: false,
      evidence: null,
      spikeShortId: null,
      spikeTaskId: null,
      claimSummary: null,
      failureReason: null,
      isPausedFrozen: false,
    };
  }

  const evidence = resolveEvidence(status, gateEvidence);
  const lifecycle = status.evidence_lifecycle_state;

  // --- Paused / frozen: highest precedence, suppress auto-resume ---
  if (lifecycle === "paused_or_frozen") {
    return {
      kind: "paused_frozen",
      badge: "Paused",
      showInProgress: false,
      suppressAutoResume: true,
      evidence,
      spikeShortId: evidence?.spike_short_id ?? null,
      spikeTaskId: evidence?.spike_task_id ?? null,
      claimSummary: evidence?.question ?? evidence?.claim ?? null,
      failureReason: evidence?.failure_reason ?? null,
      isPausedFrozen: true,
    };
  }

  // --- Awaiting review / converged: must not be reclassified ---
  if (status.awaiting_review) {
    return {
      kind: "awaiting_review",
      badge: "Awaiting review",
      showInProgress: false,
      suppressAutoResume: false,
      evidence,
      spikeShortId: null,
      spikeTaskId: null,
      claimSummary: null,
      failureReason: null,
      isPausedFrozen: false,
    };
  }

  // --- Evidence failed ---
  if (isEvidenceFailed(lifecycle, evidence)) {
    return {
      kind: "evidence_failed",
      badge: "Evidence failed",
      showInProgress: false,
      suppressAutoResume: true,
      evidence,
      spikeShortId: evidence?.spike_short_id ?? null,
      spikeTaskId: evidence?.spike_task_id ?? null,
      claimSummary: evidence?.question ?? evidence?.claim ?? null,
      failureReason: evidence?.failure_reason ?? null,
      isPausedFrozen: false,
    };
  }

  // --- Awaiting evidence ---
  if (isAwaitingEvidence(lifecycle, evidence)) {
    return {
      kind: "awaiting_evidence",
      badge: evidence?.spike_short_id
        ? `Awaiting evidence (${evidence.spike_short_id})`
        : "Awaiting evidence",
      showInProgress: false,
      suppressAutoResume: true,
      evidence,
      spikeShortId: evidence?.spike_short_id ?? null,
      spikeTaskId: evidence?.spike_task_id ?? null,
      claimSummary: evidence?.question ?? evidence?.claim ?? null,
      failureReason: null,
      isPausedFrozen: false,
    };
  }

  // --- Evidence received (transitional — refinement may resume) ---
  if (
    lifecycle === "evidence_received" ||
    evidence?.evidence_phase === "evidence_received"
  ) {
    return {
      kind: "evidence_received",
      badge: "Evidence received",
      showInProgress: true,
      suppressAutoResume: false,
      evidence,
      spikeShortId: evidence?.spike_short_id ?? null,
      spikeTaskId: evidence?.spike_task_id ?? null,
      claimSummary: evidence?.question ?? evidence?.claim ?? null,
      failureReason: null,
      isPausedFrozen: false,
    };
  }

  // --- Terminal / stopped ---
  if (
    lifecycle === "terminal" ||
    (status.active === false && status.stop_reason)
  ) {
    return {
      kind: "terminal",
      badge: statusLabelForStop(status),
      showInProgress: false,
      suppressAutoResume: false,
      evidence,
      spikeShortId: null,
      spikeTaskId: null,
      claimSummary: null,
      failureReason: null,
      isPausedFrozen: false,
    };
  }

  // --- Ordinary in-progress ---
  if (status.active) {
    return {
      kind: "in_progress",
      badge: "In progress",
      showInProgress: true,
      suppressAutoResume: false,
      evidence,
      spikeShortId: null,
      spikeTaskId: null,
      claimSummary: null,
      failureReason: null,
      isPausedFrozen: false,
    };
  }

  // --- Not started ---
  return {
    kind: "not_started",
    badge: "",
    showInProgress: false,
    suppressAutoResume: false,
    evidence: null,
    spikeShortId: null,
    spikeTaskId: null,
    claimSummary: null,
    failureReason: null,
    isPausedFrozen: false,
  };
}

// ---------------------------------------------------------------------------
// Internal label helpers
// ---------------------------------------------------------------------------

const STOP_REASON_LABELS: Record<string, string> = {
  adversary_dry: "Stopped (adversary dry)",
  round_cap: "Stopped (round cap)",
  spawn_cap: "Stopped (spawn cap)",
  repeated_objection: "Stopped (repeated objection)",
  agent_failure: "Stopped (agent failure)",
};

function statusLabelForStop(status: ProposalRefinementStatus | null): string {
  const reason = status?.stop_reason;
  if (!reason) return "Stopped";
  return STOP_REASON_LABELS[reason] ?? `Stopped (${reason})`;
}
