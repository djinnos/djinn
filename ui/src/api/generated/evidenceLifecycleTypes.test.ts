import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";
import type {
  ProposalRefinementStatusOutputSchema,
  ProposalShowOutputSchema,
} from "@/api/generated/mcp-tools.gen";
import type { NeedsEvidenceStatus, ProposalGateStatus } from "@/api/types";
import { TYPED_EVIDENCE_LIFECYCLES } from "@/components/proposals/refinementEvidenceStatus";

// ── Why this file reads its own generated source ───────────────────────────
//
// Everything that matters here is type-level, and adversarial verification of
// proposal `667e` observed the consequence: all seven runtime `it()` bodies
// compared a local literal to itself. `expect(states).toContain("evidence_failed")`
// on an array the same test had just written proves nothing about the server,
// the generated file, or the browser. Delete the one `files:` entry in
// `tsconfig.json` and 289 lines went inert with zero test failures.
//
// So the runtime half now reads the generated artifact as TEXT and asserts the
// unions it declares. That check survives losing the typecheck entirely: a
// regeneration that drops `evidence_failed` fails these tests whether or not
// anything ever runs `tsc`.

// vitest's `root` is `ui/`, so repo-relative paths resolve from the cwd.
// `import.meta.url` is not a `file:` URL under the jsdom environment.
const UI_ROOT = process.cwd();
const THIS_FILE = "src/api/generated/evidenceLifecycleTypes.test.ts";

const GENERATED_SOURCE = readFileSync(
  path.join(UI_ROOT, "src/api/generated/mcp-tools.gen.ts"),
  "utf8",
);

/** Read one of the repo's JSONC tsconfigs. */
function readTsconfig(name: string): {
  files?: string[];
  include?: string[];
  exclude?: string[];
} {
  const text = readFileSync(path.join(UI_ROOT, name), "utf8");
  return JSON.parse(text.replace(/^\s*\/\/.*$/gm, ""));
}

/**
 * The members of every `export type <name> = ("a" | "b")` declaration in the
 * generated file.
 *
 * The generator emits one copy per tool namespace. All copies must agree, or
 * the browser's view of a union depends on which namespace it imported from.
 */
function generatedUnion(name: string): string[] {
  const declarations = [
    ...GENERATED_SOURCE.matchAll(
      new RegExp(`export type ${name} =\\s*\\(([^)]*)\\)`, "g"),
    ),
  ].map((match) =>
    [...match[1].matchAll(/"([^"]+)"/g)].map((member) => member[1]).sort(),
  );
  expect(
    declarations.length,
    `${name} must be declared in the generated file`,
  ).toBeGreaterThan(0);
  for (const declaration of declarations) {
    expect(declaration, `every copy of ${name} must agree`).toEqual(
      declarations[0],
    );
  }
  return declarations[0];
}

/** The members of an inline field union such as `evidence_lifecycle_state:`. */
function generatedFieldUnion(field: string): string[] {
  const declarations = [
    ...GENERATED_SOURCE.matchAll(new RegExp(`\\n  ${field}: \\(([^)]*)\\)`, "g")),
  ].map((match) =>
    [...match[1].matchAll(/"([^"]+)"/g)].map((member) => member[1]).sort(),
  );
  expect(
    declarations.length,
    `${field} must be declared in the generated file`,
  ).toBeGreaterThan(0);
  for (const declaration of declarations) {
    expect(declaration, `every copy of ${field} must agree`).toEqual(
      declarations[0],
    );
  }
  return declarations[0];
}

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
// Still exercised, but through the generated SOURCE rather than through a
// value this test writes and then reads back. See the header.
type GenNeedsEvidenceStatus =
  ProposalRefinementStatusOutputSchema.NeedsEvidenceStatus;
const _needsEvidenceStatusIsGenerated: GenNeedsEvidenceStatus = {
  claim: "X is load-bearing",
  spike_task_id: "uuid",
  spike_short_id: "ab12",
  spike_status: "open",
  evidence_phase: "evidence_failed",
  failure_reason: "spike_force_closed",
};
void _needsEvidenceStatusIsGenerated;

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

  it("the generated EvidenceLifecyclePhase union is exactly the three phases", () => {
    expect(generatedUnion("EvidenceLifecyclePhase")).toEqual([
      "awaiting_evidence",
      "evidence_failed",
      "evidence_received",
    ]);
  });

  it("the generated evidence_lifecycle_state field is exactly the six discriminators", () => {
    expect(generatedFieldUnion("evidence_lifecycle_state")).toEqual([
      "active",
      "awaiting_evidence",
      "evidence_failed",
      "evidence_received",
      "paused_or_frozen",
      "terminal",
    ]);
  });

  it("the generated NeedsEvidenceStatus declares the legacy compatibility fields", () => {
    // Read out of the generated source, so a regeneration that drops a field
    // fails here even with no typecheck in the pipeline at all.
    const declaration = /export interface NeedsEvidenceStatus \{([\s\S]*?)\n {2}\}/.exec(
      GENERATED_SOURCE,
    );
    expect(declaration, "NeedsEvidenceStatus must exist in the generated file")
      .not.toBeNull();
    const fields = [
      ...declaration![1].matchAll(/^ {2}([a-z_]+)\??:/gm),
    ].map((match) => match[1]);
    for (const field of [
      "claim",
      "evidence_phase",
      "failure_reason",
      "spike_short_id",
      "spike_status",
      "spike_task_id",
    ]) {
      expect(fields, `generated NeedsEvidenceStatus.${field}`).toContain(field);
    }
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

  it("the browser's pinned lifecycle list is exactly the generated union", () => {
    // The left side is the runtime constant the classifier is driven over; the
    // right side is read out of the generated file. Neither is written in this
    // test, so a state added to one and not the other reddens here.
    expect([...TYPED_EVIDENCE_LIFECYCLES].sort()).toEqual(
      generatedUnion("TypedEvidenceLifecycleModel"),
    );
    expect(TYPED_EVIDENCE_LIFECYCLES).toHaveLength(6);
  });

  it("the generated typed outcome union is exactly the three outcomes", () => {
    expect(generatedUnion("TypedEvidenceOutcomeModel")).toEqual([
      "partial",
      "resolved",
      "unresolved",
    ]);
  });

  it("typed lifecycle is wider than the legacy three-phase vocabulary", () => {
    // `resolved`, `withdrawn`, `demanded`, `spike_active` and `failed` are
    // states the legacy phase enum has no representation for. Conflating the
    // two vocabularies is the drift this guard exists to catch — and both
    // sides now come out of the generated file rather than out of this test.
    const legacy = generatedUnion("EvidenceLifecyclePhase");
    const typed = generatedUnion("TypedEvidenceLifecycleModel");
    expect(typed.filter((state) => legacy.includes(state))).toEqual([
      "evidence_received",
    ]);
  });

  it("is compiled by a tsconfig CI runs, because its real assertions are compile-time", () => {
    // The type-level guards above produce no runtime failure of any kind. If
    // nothing in CI compiles this file they are decoration, which is exactly
    // what `pnpm tsc --noEmit` excluding every `*.test.ts` used to mean.
    const root = readTsconfig("tsconfig.json");
    const gate = readTsconfig("tsconfig.test-gate.json");
    const inRootFiles = (root.files ?? []).includes(THIS_FILE);
    const inGate =
      (gate.include ?? []).some((pattern) =>
        THIS_FILE.startsWith(`${pattern}/`),
      ) && !(gate.exclude ?? []).includes(THIS_FILE);
    expect(
      inRootFiles || inGate,
      "this file must stay in tsconfig.json's `files` (gated by `pnpm tsc --noEmit`) " +
        "or inside tsconfig.test-gate.json (gated by `pnpm test:typecheck:gate`)",
    ).toBe(true);
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
