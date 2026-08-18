import type {
  ProposalRefinementStatus,
  NeedsEvidenceStatus,
  TypedEvidenceGateStatus,
  TypedEvidenceLifecycle,
  TypedEvidenceOutcome,
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
  /**
   * Classification of the *typed* evidence authority, which is separate from
   * every legacy field above and never folded into `kind`. A reader can always
   * tell which authority produced a decision. `typed.kind` is `typed_clear`
   * with no actions when the server published no typed section.
   */
  typed: TypedEvidenceDisplay;
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
  if (evidence.evidence_phase === "evidence_failed") return true;
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
 * This is the single evidence classifier: it covers the legacy `needs_evidence`
 * lifecycle **and** the typed authority, so no renderer has to branch on raw
 * status fields or decide for itself which authority is speaking.
 *
 * @param status  The proposal's refinement status (may be `null`).
 * @param gateEvidence  Optional gate-level `needs_evidence` from
 *   `ProposalGateStatus`, used as fallback when `status.needs_evidence` is
 *   absent.
 * @param typedEvidence  The gate's typed evidence section, when the server
 *   published one. Action visibility is derived from this and nothing else.
 */
export function classifyRefinementEvidence(
  status: ProposalRefinementStatus | null,
  gateEvidence?: NeedsEvidenceStatus | null,
  typedEvidence?: TypedEvidenceGateStatus | null,
): RefinementEvidenceDisplay {
  return {
    ...classifyLegacyRefinementEvidence(status, gateEvidence),
    typed: classifyTypedEvidence(typedEvidence),
  };
}

/**
 * The legacy half of [`classifyRefinementEvidence`]. Private: callers must go
 * through the single classifier so a renderer cannot accidentally read the
 * legacy authority while a typed finding is open.
 */
function classifyLegacyRefinementEvidence(
  status: ProposalRefinementStatus | null,
  gateEvidence?: NeedsEvidenceStatus | null,
): Omit<RefinementEvidenceDisplay, "typed"> {
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

// ---------------------------------------------------------------------------
// Typed evidence lifecycle (zzbp)
// ---------------------------------------------------------------------------
//
// The typed lifecycle is a *different* authority from the legacy
// `evidence_phase` handled above, with a wider vocabulary and its own
// blocking rule. It is classified separately and never folded into the legacy
// kinds, so a reader can always tell which authority produced a decision.

/**
 * Display classification for a typed evidence finding.
 *
 * The four blocking states each get their own kind — a reviewer looking at a
 * blocked gate has to be able to tell "no spike has been dispatched yet" from
 * "the spike returned something the server rejected". The two terminal states
 * share `typed_clear`: neither blocks, and neither has an action.
 */
export type TypedEvidenceDisplayKind =
  | "typed_demanded"
  | "typed_spike_active"
  | "typed_evidence_received"
  | "typed_failed"
  | "typed_clear";

/**
 * The complete set of actions the browser may offer for a typed finding.
 *
 * `resolve` and `withdraw` are present and permanently `false`. They are named
 * rather than omitted because "the UI exposes no resolve/withdraw affordance"
 * is a contract worth asserting on: a Judge disposition is a server-side
 * decision that must be recorded with a rationale and a folding revision, and
 * there is no browser affordance that could supply either honestly.
 */
export interface TypedEvidenceActions {
  /**
   * Retry the finding's latest failed attempt. `true` only when the finding
   * is `failed` **and** the server projected retry authority for this caller.
   */
  retry: boolean;
  /** Always `false`. See the type doc. */
  resolve: false;
  /** Always `false`. See the type doc. */
  withdraw: false;
}

/** Display-safe data derived from the server's typed evidence projection. */
export interface TypedEvidenceDisplay {
  kind: TypedEvidenceDisplayKind;
  /** Badge / ribbon label. */
  badge: string;
  /**
   * Whether this finding refuses transitions. Read from the *server's*
   * `blocking` flag when present; the four unresolved lifecycle states are
   * the browser's fallback classification when the server did not say.
   */
  blocking: boolean;
  /** The action affordances, derived only from server-projected authority. */
  actions: TypedEvidenceActions;
  /** The finding id, or `null` when there is no typed finding. */
  findingId: string | null;
  /**
   * The failed transition a retry must cite. `null` unless the server named
   * one — the browser never synthesizes it.
   */
  failedTransitionId: string | null;
  /** Validated outcome of the latest return, or `null`. */
  evidenceOutcome: TypedEvidenceOutcome | null;
}

/**
 * The whole typed lifecycle vocabulary, pinned as a value so the classifier
 * can be driven over it and so a fixture cannot smuggle in a state nobody
 * handled. `EXHAUSTIVE_LIFECYCLES` is checked against the generated union
 * below: if the server adds a state, this file stops compiling.
 */
export const TYPED_EVIDENCE_LIFECYCLES = [
  "demanded",
  "spike_active",
  "evidence_received",
  "failed",
  "resolved",
  "withdrawn",
] as const;

// Both directions. A state added to the generated union but missing here, or
// a state invented here that the server does not publish, is a compile error.
type PinnedLifecycle = (typeof TYPED_EVIDENCE_LIFECYCLES)[number];
type LifecyclesAreExactlyPinned =
  [TypedEvidenceLifecycle] extends [PinnedLifecycle]
    ? [PinnedLifecycle] extends [TypedEvidenceLifecycle]
      ? true
      : never
    : never;
const _lifecyclesAreExactlyPinned: LifecyclesAreExactlyPinned = true;
void _lifecyclesAreExactlyPinned;

/** The four states that hold a proposal's gate closed. */
export const BLOCKING_TYPED_LIFECYCLES: readonly TypedEvidenceLifecycle[] = [
  "demanded",
  "spike_active",
  "evidence_received",
  "failed",
];

/** Runtime narrowing for values that arrive as untyped JSON (fixtures, wire). */
export function isTypedEvidenceLifecycle(
  value: unknown,
): value is TypedEvidenceLifecycle {
  return (
    typeof value === "string" &&
    (TYPED_EVIDENCE_LIFECYCLES as readonly string[]).includes(value)
  );
}

const TYPED_LIFECYCLE_PRESENTATION: Record<
  TypedEvidenceLifecycle,
  { kind: TypedEvidenceDisplayKind; badge: string }
> = {
  demanded: { kind: "typed_demanded", badge: "Evidence demanded" },
  spike_active: { kind: "typed_spike_active", badge: "Evidence spike running" },
  evidence_received: {
    kind: "typed_evidence_received",
    badge: "Evidence under review",
  },
  failed: { kind: "typed_failed", badge: "Evidence failed" },
  resolved: { kind: "typed_clear", badge: "Evidence resolved" },
  withdrawn: { kind: "typed_clear", badge: "Demand withdrawn" },
};

/** The display for a proposal with no typed finding at all. */
const NO_TYPED_FINDING: TypedEvidenceDisplay = {
  kind: "typed_clear",
  badge: "",
  blocking: false,
  actions: { retry: false, resolve: false, withdraw: false },
  findingId: null,
  failedTransitionId: null,
  evidenceOutcome: null,
};

/**
 * Classify the server's typed evidence section into display-safe data.
 *
 * Action visibility is derived **only** from what the server projected. The
 * browser does not consult the caller's role, the proposal's status, or any
 * local state: `retry_permitted` is the server answering "would I admit a
 * retry from this caller right now", and `failed_transition_id` is the exact
 * transition the write path demands. Deriving either locally would produce a
 * button that the server rejects.
 *
 * @throws when `lifecycle` is a value this classifier does not handle. That is
 *   deliberate: falling through to a default would render an unknown blocking
 *   state as a clean one, which is the failure mode that silently unblocks a
 *   gate. The `never` binding below makes the same mistake a compile error for
 *   any typed caller; the throw covers untyped JSON.
 */
export function classifyTypedEvidence(
  typed: TypedEvidenceGateStatus | null | undefined,
): TypedEvidenceDisplay {
  if (!typed || typed.lifecycle === undefined || typed.lifecycle === null) {
    return NO_TYPED_FINDING;
  }
  const lifecycle = typed.lifecycle;
  if (!isTypedEvidenceLifecycle(lifecycle)) {
    throw new Error(
      `unhandled typed evidence lifecycle: ${JSON.stringify(lifecycle)}`,
    );
  }
  let presentation: { kind: TypedEvidenceDisplayKind; badge: string };
  switch (lifecycle) {
    case "demanded":
    case "spike_active":
    case "evidence_received":
    case "failed":
    case "resolved":
    case "withdrawn":
      presentation = TYPED_LIFECYCLE_PRESENTATION[lifecycle];
      break;
    default: {
      // Unreachable for any well-typed caller. If the server adds a lifecycle
      // state, this binding stops compiling before it can reach a renderer.
      const unhandled: never = lifecycle;
      throw new Error(
        `unhandled typed evidence lifecycle: ${JSON.stringify(unhandled)}`,
      );
    }
  }

  // Retry is the one permitted action, and it needs BOTH halves: the finding
  // must actually be failed, and the server must have said this caller may
  // retry it. Either half alone renders nothing.
  const retry =
    lifecycle === "failed" &&
    typed.retry_permitted === true &&
    typeof typed.failed_transition_id === "string" &&
    typed.failed_transition_id.length > 0;

  return {
    kind: presentation.kind,
    badge: presentation.badge,
    // The server's own refusal flag wins; the lifecycle set is the fallback
    // for a projection that predates the flag.
    blocking:
      typeof typed.blocking === "boolean"
        ? typed.blocking
        : BLOCKING_TYPED_LIFECYCLES.includes(lifecycle),
    actions: { retry, resolve: false, withdraw: false },
    findingId: typeof typed.finding_id === "string" ? typed.finding_id : null,
    failedTransitionId:
      typeof typed.failed_transition_id === "string"
        ? typed.failed_transition_id
        : null,
    evidenceOutcome: (typed.evidence_outcome ?? null) as
      | TypedEvidenceOutcome
      | null,
  };
}
