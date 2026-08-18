import { describe, expect, it } from "vitest";
import type {
  ProposalRefinementStatus,
  NeedsEvidenceStatus,
  TypedEvidenceGateStatus,
} from "@/api/types";
import {
  BLOCKING_TYPED_LIFECYCLES,
  TYPED_EVIDENCE_LIFECYCLES,
  classifyRefinementEvidence,
} from "./refinementEvidenceStatus";
import actionMatrix from "./__fixtures__/typed-evidence-action-matrix.json";

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

// ---------------------------------------------------------------------------
// Typed evidence action matrix (zzbp)
// ---------------------------------------------------------------------------
//
// The fixture is the contract. Every row is driven through the single
// classifier, and the axes are asserted to be complete before the rows run —
// a matrix that quietly lost its `retry_permitted: true` half would otherwise
// pass while proving nothing about the capability axis.
//
// The fourth axis is not decoration. `presentation_for_finding` projects the
// latest `-> failed` transition from the append-only transition log, so a
// finding that failed once and was then retried is `spike_active` while still
// carrying a real failed transition id. Without that axis, a classifier that
// dropped the lifecycle check entirely would still pass every row.

interface MatrixRow {
  name: string;
  gate: Record<string, unknown>;
  expect: {
    kind: string;
    blocking: boolean;
    retry: boolean;
    resolve: boolean;
    withdraw: boolean;
  };
}

const matrix = actionMatrix as unknown as {
  lifecycles: string[];
  outcomes: string[];
  rows: MatrixRow[];
};

/** Classify one fixture row through the single public classifier. */
function classifyRow(row: MatrixRow) {
  return classifyRefinementEvidence(
    activeStatus,
    undefined,
    row.gate as unknown as TypedEvidenceGateStatus,
  );
}

describe("typed evidence action matrix", () => {
  // ---- The matrix itself is complete -------------------------------------

  it("covers every lifecycle x outcome x capability x failed-transition combination", () => {
    // The lifecycle axis must be the whole pinned vocabulary, not a subset
    // someone trimmed. `TYPED_EVIDENCE_LIFECYCLES` is itself pinned to the
    // generated union at compile time in refinementEvidenceStatus.ts.
    expect([...matrix.lifecycles].sort()).toEqual(
      [...TYPED_EVIDENCE_LIFECYCLES].sort(),
    );
    expect([...matrix.outcomes].sort()).toEqual(
      ["none", "partial", "resolved", "unresolved"].sort(),
    );

    const key = (row: MatrixRow) =>
      [
        String(row.gate.lifecycle),
        String(row.gate.evidence_outcome ?? "none"),
        String(row.gate.retry_permitted),
        String(typeof row.gate.failed_transition_id === "string"),
      ].join("|");
    const seen = new Set(matrix.rows.map(key));
    const wanted: string[] = [];
    for (const lifecycle of matrix.lifecycles) {
      for (const outcome of matrix.outcomes) {
        for (const permitted of ["false", "true"]) {
          for (const hasTransition of ["false", "true"]) {
            wanted.push(`${lifecycle}|${outcome}|${permitted}|${hasTransition}`);
          }
        }
      }
    }
    expect([...seen].sort()).toEqual([...wanted].sort());
    expect(wanted).toHaveLength(6 * 4 * 2 * 2);
    expect(matrix.rows).toHaveLength(wanted.length);
  });

  // ---- Every row is actually driven --------------------------------------

  it("drives every fixture row through classifyRefinementEvidence", () => {
    const driven: string[] = [];
    for (const row of matrix.rows) {
      const display = classifyRow(row);
      driven.push(row.name);
      expect(display.typed.kind, row.name).toBe(row.expect.kind);
      expect(display.typed.blocking, row.name).toBe(row.expect.blocking);
      expect(display.typed.actions.retry, row.name).toBe(row.expect.retry);
      expect(display.typed.actions.resolve, row.name).toBe(row.expect.resolve);
      expect(display.typed.actions.withdraw, row.name).toBe(
        row.expect.withdraw,
      );
      // Identity and provenance are carried through verbatim — the classifier
      // never invents or reformats either.
      expect(display.typed.findingId, row.name).toBe(row.gate.finding_id);
      expect(display.typed.failedTransitionId, row.name).toBe(
        row.gate.failed_transition_id ?? null,
      );
      expect(display.typed.evidenceOutcome, row.name).toBe(
        row.gate.evidence_outcome ?? null,
      );
    }
    // A row that threw or was skipped would not reach here.
    expect(driven).toHaveLength(matrix.rows.length);
    expect(driven.length).toBe(96);
  });

  // ---- AC2: retry needs the lifecycle AND server-projected permission -----

  it("shows retry only for a failed finding the server says this caller may retry", () => {
    const shown = matrix.rows.filter(
      (row) => classifyRow(row).typed.actions.retry,
    );
    for (const row of shown) {
      expect(row.gate.lifecycle, row.name).toBe("failed");
      expect(row.gate.retry_permitted, row.name).toBe(true);
      expect(typeof row.gate.failed_transition_id, row.name).toBe("string");
    }
    // Not vacuous: every eligible row does show it.
    const eligible = matrix.rows.filter(
      (row) =>
        row.gate.lifecycle === "failed" &&
        row.gate.retry_permitted === true &&
        typeof row.gate.failed_transition_id === "string",
    );
    expect(eligible.length).toBe(4);
    expect(shown).toHaveLength(eligible.length);

    // Each half alone is insufficient.
    for (const row of matrix.rows) {
      const display = classifyRow(row);
      if (row.gate.lifecycle !== "failed") {
        expect(display.typed.actions.retry, `non-failed: ${row.name}`).toBe(
          false,
        );
      }
      if (row.gate.retry_permitted !== true) {
        expect(display.typed.actions.retry, `unpermitted: ${row.name}`).toBe(
          false,
        );
      }
    }
  });

  it("keeps retry hidden on a non-failed finding that still carries a historical failed transition", () => {
    // The exact shape a retried finding has: `spike_active`, an authorized
    // caller, and a real failed transition id left behind by the earlier
    // attempt. Offering retry here would double-dispatch a running spike.
    const rows = matrix.rows.filter(
      (row) =>
        row.gate.lifecycle === "spike_active" &&
        row.gate.retry_permitted === true &&
        typeof row.gate.failed_transition_id === "string",
    );
    expect(rows.length).toBeGreaterThan(0);
    for (const row of rows) {
      const display = classifyRow(row);
      expect(display.typed.failedTransitionId, row.name).toBe(
        row.gate.failed_transition_id,
      );
      expect(display.typed.actions.retry, row.name).toBe(false);
    }
  });

  it("withholds retry when the server granted permission but named no failed transition", () => {
    const rows = matrix.rows.filter(
      (row) =>
        row.gate.lifecycle === "failed" &&
        row.gate.retry_permitted === true &&
        row.gate.failed_transition_id === undefined,
    );
    expect(rows.length).toBe(4);
    for (const row of rows) {
      const display = classifyRow(row);
      expect(display.typed.kind, row.name).toBe("typed_failed");
      expect(display.typed.failedTransitionId, row.name).toBeNull();
      // The write path demands the transition id, so a retry the browser
      // cannot construct must not be offered.
      expect(display.typed.actions.retry, row.name).toBe(false);
    }
  });

  // ---- AC3: no resolve/withdraw affordance, anywhere ----------------------

  it("exposes no resolve or withdraw affordance on any row", () => {
    for (const row of matrix.rows) {
      const actions = classifyRow(row).typed.actions;
      expect(actions.resolve, row.name).toBe(false);
      expect(actions.withdraw, row.name).toBe(false);
    }
    // The action surface is closed: retry/resolve/withdraw and nothing else,
    // so a future action cannot be added without updating this assertion.
    expect(
      Object.keys(classifyRow(matrix.rows[0]).typed.actions).sort(),
    ).toEqual(["resolve", "retry", "withdraw"]);
  });

  // ---- AC4: the four blocking states are distinct from the clean states ---

  it("maps each of the four blocking lifecycles to its own kind, distinct from the clean states", () => {
    const kindFor = (lifecycle: string) => {
      const row = matrix.rows.find((r) => r.gate.lifecycle === lifecycle);
      expect(row, `fixture must carry a ${lifecycle} row`).toBeDefined();
      return classifyRow(row!).typed.kind;
    };
    const blockingKinds = BLOCKING_TYPED_LIFECYCLES.map(kindFor);
    expect(blockingKinds).toEqual([
      "typed_demanded",
      "typed_spike_active",
      "typed_evidence_received",
      "typed_failed",
    ]);
    // Four states, four distinct kinds.
    expect(new Set(blockingKinds).size).toBe(4);

    const cleanKinds = ["resolved", "withdrawn"].map(kindFor);
    expect(new Set(cleanKinds)).toEqual(new Set(["typed_clear"]));
    for (const kind of blockingKinds) {
      expect(cleanKinds).not.toContain(kind);
    }

    // And every blocking-lifecycle row reports blocking=true, while the
    // terminal ones report false.
    for (const row of matrix.rows) {
      const expectedBlocking = (
        BLOCKING_TYPED_LIFECYCLES as readonly string[]
      ).includes(String(row.gate.lifecycle));
      expect(classifyRow(row).typed.blocking, row.name).toBe(expectedBlocking);
    }
  });

  it("reports the server's blocking flag, not a locally recomputed one", () => {
    // Shadow mode: the server surfaces an unresolved `demanded` finding but is
    // not refusing transitions on it yet. The lifecycle would say "blocking";
    // the server says otherwise, and the server wins. The display kind still
    // reports which lifecycle state it is, so the two facts stay separable.
    const shadow = classifyRefinementEvidence(activeStatus, undefined, {
      mode: "shadow",
      blocking: false,
      finding_id: "finding-shadow",
      lifecycle: "demanded",
      demanded_revision_seq: 1,
      attempts: [],
      planned_checks: [],
      gaps: [],
      usable_findings: [],
      retry_permitted: false,
    } as unknown as TypedEvidenceGateStatus);
    expect(shadow.typed.kind).toBe("typed_demanded");
    expect(shadow.typed.blocking).toBe(false);

    // And the converse: a terminal lifecycle the server still reports as
    // blocking (a fail-closed parity refusal) is rendered as blocking.
    const failClosed = classifyRefinementEvidence(activeStatus, undefined, {
      mode: "enforce",
      blocking: true,
      finding_id: "finding-parity",
      lifecycle: "resolved",
      parity_mismatch_reason: "typed_evidence_parity_mismatch",
      demanded_revision_seq: 1,
      attempts: [],
      planned_checks: [],
      gaps: [],
      usable_findings: [],
      retry_permitted: false,
    } as unknown as TypedEvidenceGateStatus);
    expect(failClosed.typed.kind).toBe("typed_clear");
    expect(failClosed.typed.blocking).toBe(true);
  });

  // ---- AC5: an unhandled lifecycle fails, it does not default ------------

  it("throws on a fixture row carrying a lifecycle nobody handled", () => {
    const smuggled: MatrixRow = {
      name: "smuggled lifecycle",
      gate: { ...matrix.rows[0].gate, lifecycle: "escalated_to_a_human" },
      expect: {
        kind: "typed_clear",
        blocking: false,
        retry: false,
        resolve: false,
        withdraw: false,
      },
    };
    // The failure mode this guards is rendering an unknown blocking state as a
    // clean one, which silently unblocks a gate in the reviewer's eyes.
    expect(() => classifyRow(smuggled)).toThrow(
      /unhandled typed evidence lifecycle/,
    );
    // Free text cannot ride in on the lifecycle field either.
    expect(() =>
      classifyRow({
        ...smuggled,
        gate: { ...smuggled.gate, lifecycle: "the judge is still thinking" },
      }),
    ).toThrow(/unhandled typed evidence lifecycle/);
  });

  it("reports no typed finding as typed_clear with no actions", () => {
    for (const absent of [undefined, null, {}]) {
      const display = classifyRefinementEvidence(
        activeStatus,
        undefined,
        absent as unknown as TypedEvidenceGateStatus,
      );
      expect(display.typed.kind).toBe("typed_clear");
      expect(display.typed.blocking).toBe(false);
      expect(display.typed.actions).toEqual({
        retry: false,
        resolve: false,
        withdraw: false,
      });
      expect(display.typed.findingId).toBeNull();
      // The legacy classification is untouched by the typed section's absence.
      expect(display.kind).toBe("in_progress");
    }
  });
});
