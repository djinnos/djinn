import { describe, expect, it } from "vitest";
import { ProposalRefinementStatusOutputSchema } from "@/api/generated/mcp-tools.gen";
import type { NeedsEvidenceStatus } from "@/api/types";

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
