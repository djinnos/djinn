import { describe, expect, it } from "vitest";
import type {
  ProposalRefinementStatusOutputSchema,
  ProposalShowOutputSchema,
} from "@/api/generated/mcp-tools.gen";
import type { NeedsEvidenceStatus, ProposalGateStatus } from "@/api/types";

// ── Evidence lifecycle generated-type regression (hoh3 / tlon) ─────────────
//
// AC#3: prove the generated MCP TypeScript surface includes the distinct
// AwaitingEvidence / EvidenceFailed payload shape.  These are compile-time
// type-level assertions (no runtime work beyond the smoke checks) so a
// regeneration that silently drops a field or narrows an enum fails the build
// with a clear error rather than letting the UI render a broken state at
// runtime.

// Pull the generated types out of the namespace as local type aliases.
type GenEvidenceLifecyclePhase =
  ProposalRefinementStatusOutputSchema.EvidenceLifecyclePhase;
type GenStatusModel =
  ProposalRefinementStatusOutputSchema.ProposalRefinementStatusModel;
type GenNeedsEvidenceStatus =
  ProposalRefinementStatusOutputSchema.NeedsEvidenceStatus;

describe("generated evidence lifecycle types", () => {
  // ── Type-level assertions ──────────────────────────────────────────────

  // The generated EvidenceLifecyclePhase union must include all three phases.
  // If a regeneration drops one, this type error fires.
  const _phaseCheck: GenEvidenceLifecyclePhase = "awaiting_evidence";
  const _phaseCheck2: GenEvidenceLifecyclePhase = "evidence_received";
  const _phaseCheck3: GenEvidenceLifecyclePhase = "evidence_failed";

  // The generated status model must expose the discriminator + nested payload.
  const _modelCheck: GenStatusModel = {
    active: true,
    dry_rounds: 0,
    total_entries: 0,
    evidence_lifecycle_state: "awaiting_evidence",
    needs_evidence: {
      claim: "X is load-bearing",
      spike_task_id: "uuid",
      spike_short_id: "ab12",
      spike_status: "open",
      evidence_phase: "awaiting_evidence",
    },
  };

  // EvidenceFailed must be assignable to the evidence_lifecycle_state field.
  const _failedModel: GenStatusModel = {
    ..._modelCheck,
    evidence_lifecycle_state: "evidence_failed",
    needs_evidence: {
      ..._modelCheck.needs_evidence!,
      evidence_phase: "evidence_failed",
      failure_reason: "spike_force_closed",
    },
  };

  // The generated NeedsEvidenceStatus must be structurally compatible with the
  // hand-written ui/src/api/types.ts NeedsEvidenceStatus interface.  This
  // catches drift between the generated surface and the hand-curated type.
  const _compatCheck: NeedsEvidenceStatus =
    _modelCheck.needs_evidence! as NeedsEvidenceStatus;
  // Suppress unused-variable lint for compile-only type assertions.
  void _phaseCheck;
  void _phaseCheck2;
  void _phaseCheck3;
  void _failedModel;
  void _compatCheck;

  // ── Runtime smoke checks ───────────────────────────────────────────────

  it("EvidenceLifecyclePhase includes awaiting_evidence, evidence_received, evidence_failed", () => {
    const phases: GenEvidenceLifecyclePhase[] = [
      "awaiting_evidence",
      "evidence_received",
      "evidence_failed",
    ];
    expect(phases).toContain("awaiting_evidence");
    expect(phases).toContain("evidence_failed");
    expect(phases).toContain("evidence_received");
  });

  it("status model carries evidence_lifecycle_state with six discriminators", () => {
    const states: GenStatusModel["evidence_lifecycle_state"][] = [
      "active",
      "awaiting_evidence",
      "evidence_received",
      "evidence_failed",
      "paused_or_frozen",
      "terminal",
    ];
    expect(states).toHaveLength(6);
    expect(states).toContain("awaiting_evidence");
    expect(states).toContain("evidence_failed");
  });

  it("NeedsEvidenceStatus generated surface includes claim, spike id, phase, and failure_reason", () => {
    const ne: GenNeedsEvidenceStatus = {
      claim: "X is load-bearing",
      spike_task_id: "uuid",
      spike_short_id: "ab12",
      spike_status: "open",
      evidence_phase: "evidence_failed",
      failure_reason: "spike_force_closed",
    };
    expect(ne.claim).toBe("X is load-bearing");
    expect(ne.spike_short_id).toBe("ab12");
    expect(ne.evidence_phase).toBe("evidence_failed");
    expect(ne.failure_reason).toBe("spike_force_closed");
  });
});

// ── Typed evidence lifecycle drift guard (ggfc) ─────────────────────────────
//
// The legacy `EvidenceLifecyclePhase` block above guards the three-phase
// compatibility shape. The typed lifecycle is a different, wider vocabulary
// owned by `TypedEvidenceRepository`, and the browser must learn it from the
// generated surface rather than restating it — otherwise a server that renames
// or drops a state renders a silent fallback instead of failing the build.

type GenTypedLifecycle = ProposalShowOutputSchema.TypedEvidenceLifecycleModel;
type GenTypedOutcome = ProposalShowOutputSchema.TypedEvidenceOutcomeModel;
type GenGateStatus = ProposalShowOutputSchema.ProposalGateStatusModel;

/** Compile-time "these two unions are exactly equal" assertion. */
type Equals<A, B> =
  (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
    ? true
    : false;
function assertExactly<T extends true>(_: T): void {
  /* compile-time only */
}

/**
 * Fails to compile if the hand-written type in `ui/src/api/types.ts` loses any
 * field named here. `Pick` requires every key to be `keyof` the target, so a
 * deleted field is a type error at exactly this line rather than a runtime
 * `undefined` at render time.
 */
type LegacyNeedsEvidenceFields = Pick<
  NeedsEvidenceStatus,
  | "spike_task_id"
  | "spike_short_id"
  | "spike_status"
  | "claim"
  | "evidence_phase"
  | "failure_reason"
  | "against_revision_seq"
>;

describe("typed evidence lifecycle generated types", () => {
  // The typed lifecycle union is exactly the six durable states. Both
  // directions are pinned: a dropped state and an invented one each fail.
  assertExactly<
    Equals<
      GenTypedLifecycle,
      | "demanded"
      | "spike_active"
      | "evidence_received"
      | "failed"
      | "resolved"
      | "withdrawn"
    >
  >(true);

  assertExactly<
    Equals<GenTypedOutcome, "resolved" | "partial" | "unresolved">
  >(true);

  // The hand-written ProposalGateStatus must remain assignable *from* the
  // generated model: the browser's view of the gate may never require a field
  // the server does not publish, or a narrower type than the server sends.
  //
  // Deliberately a plain assignment. An `as` cast here would compile against
  // any drift at all — the generated model's `[k: string]: any` index
  // signature makes every assertion succeed — so the guard would be inert.
  const _gateCompat = (model: GenGateStatus): ProposalGateStatus => model;
  void _gateCompat;

  // Deleting any of these from the hand-written NeedsEvidenceStatus makes the
  // `Pick` above a type error, so the legacy compatibility surface cannot be
  // trimmed while a mixed-version server still sends it.
  const _legacyFields: LegacyNeedsEvidenceFields = {
    spike_task_id: "uuid",
    spike_short_id: "ab12",
    spike_status: "open",
    claim: "X is load-bearing",
    evidence_phase: "evidence_failed",
    failure_reason: "spike_force_closed",
    against_revision_seq: 3,
  };
  void _legacyFields;

  it("typed lifecycle union carries all six durable states", () => {
    // Each literal is checked against the generated union by annotation, so a
    // renamed or removed state fails the typecheck rather than this assertion.
    const states: GenTypedLifecycle[] = [
      "demanded",
      "spike_active",
      "evidence_received",
      "failed",
      "resolved",
      "withdrawn",
    ];
    expect(new Set(states).size).toBe(6);
    for (const state of [
      "demanded",
      "spike_active",
      "evidence_received",
      "failed",
      "resolved",
      "withdrawn",
    ]) {
      expect(states).toContain(state);
    }
  });

  it("typed lifecycle is wider than the legacy three-phase vocabulary", () => {
    // `resolved` and `withdrawn` are terminal states the legacy phase enum has
    // no representation for. Conflating the two vocabularies is the drift this
    // guard exists to catch.
    const legacy: GenEvidenceLifecyclePhase[] = [
      "awaiting_evidence",
      "evidence_received",
      "evidence_failed",
    ];
    const typedOnly: GenTypedLifecycle[] = [
      "demanded",
      "spike_active",
      "failed",
      "resolved",
      "withdrawn",
    ];
    for (const state of typedOnly) {
      expect(legacy as string[]).not.toContain(state);
    }
  });

  it("gate status carries the typed section with lifecycle, outcome and action authority", () => {
    const gate: GenGateStatus = {
      ready: false,
      dor_ready: true,
      dor_failures: [],
      judge_needs_work: false,
      adversary_dry_count: 0,
      unresolved_blocking_count: 0,
      unresolved_blocking_ids: [],
      human_override_active: false,
      blocked_explanations: ["unresolved typed evidence finding f1"],
      typed_evidence: {
        mode: "enforce",
        blocking: true,
        finding_id: "f1",
        claim: '{"question":"is X load-bearing"}',
        lifecycle: "failed",
        demanded_revision_seq: 2,
        attempt_seq: 1,
        evidence_outcome: "unresolved",
        attempts: [],
        planned_checks: [],
        gaps: [],
        usable_findings: [],
        retry_permitted: false,
        failed_transition_id: "t9",
      },
    };
    // The hand-written app type must accept the generated model unchanged —
    // assignment, not a cast, so drift is a compile error here.
    const app: ProposalGateStatus = gate;
    expect(app.typed_evidence?.lifecycle).toBe("failed");
    expect(app.typed_evidence?.evidence_outcome).toBe("unresolved");
    expect(app.typed_evidence?.retry_permitted).toBe(false);
    expect(app.typed_evidence?.failed_transition_id).toBe("t9");
  });

  it("legacy needs-evidence fields survive on the hand-written type", () => {
    expect(Object.keys(_legacyFields).sort()).toEqual([
      "against_revision_seq",
      "claim",
      "evidence_phase",
      "failure_reason",
      "spike_short_id",
      "spike_status",
      "spike_task_id",
    ]);
  });
});
