/**
 * Shared fixtures for the Proposals Storybook stories (list + detail building
 * blocks). Kept in one place so the page-level `Proposals/ProposalsPage`
 * stories and the per-component stories draw from the same realistic data.
 *
 * These are plain data objects typed against the real app/MCP types, so the
 * component prop shapes stay honest and tsc catches drift.
 */

import type { OrgUser } from "@/api/users";
import type {
  Proposal,
  ProposalDebateTrailRow,
  ProposalEpic,
  ProposalFeedback,
  ProposalGateStatus,
  ProposalListRow,
  ProposalListSummary,
  ProposalRefinementStatus,
  ProposalSignoff,
  ProposalTarget,
} from "@/api/types";
import type { ProposalDetail, ProposalHistoryEntry } from "@/lib/proposalQueries";

// ── Time helpers ─────────────────────────────────────────────────────────────

const ISO = (msAgo: number): string => new Date(Date.now() - msAgo).toISOString();
const minutes = (n: number) => n * 60 * 1000;
const hours = (n: number) => n * 60 * minutes(1);
const days = (n: number) => n * 24 * hours(1);

// ── Users ────────────────────────────────────────────────────────────────────

export const users: OrgUser[] = [
  {
    id: "u-alice",
    github_login: "alice",
    github_name: "Alice Rivera",
    github_avatar_url: null,
    is_member_of_org: true,
    is_admin: true,
    role: "engineer",
  },
  {
    id: "u-bob",
    github_login: "bobpm",
    github_name: "Bob Chen",
    github_avatar_url: null,
    is_member_of_org: true,
    is_admin: false,
    role: "pm",
  },
  {
    id: "u-carol",
    github_login: "carol",
    github_name: "Carol Okafor",
    github_avatar_url: null,
    is_member_of_org: true,
    is_admin: false,
    role: "proposer",
  },
];

// ── Targets ──────────────────────────────────────────────────────────────────

export const targets: ProposalTarget[] = [
  {
    created_at: ISO(days(6)),
    project_id: "project-djinn",
    project_name: "djinnos/djinn",
    project_path: "djinnos/djinn",
    role: "primary",
  },
  {
    created_at: ISO(days(6)),
    project_id: "project-catalog",
    project_name: "djinnos/catalog",
    project_path: "djinnos/catalog",
    role: "reference",
  },
];

// ── Spec revisions / history ─────────────────────────────────────────────────

const SPEC_V1 = `## Problem

The proposals list has no visual coverage of the tribunal, so reviewers open
each proposal to learn its readiness state.

## Approach

Surface a compact tribunal chip and a gate dot on every list row.`;

const SPEC_V2 = `## Problem

The proposals list has no visual coverage of the tribunal, so reviewers open
each proposal to learn its readiness state.

## Approach

Surface a compact tribunal chip and a gate dot on every list row, and float
awaiting-review rows to the top of their status group.

## Open questions

- Does the batched summary cost stay within the list query budget?`;

const SPEC_V3 = `## Problem

The proposals list has no visual coverage of the tribunal, so reviewers open
each proposal to learn its readiness state.

## Approach

Surface a compact tribunal chip and a gate dot on every list row, float
awaiting-review rows to the top of their status group, and add a per-row
unresolved-feedback count.

## Open questions

- Does the batched summary cost stay within the list query budget?
- Should terminal proposals skip the summary entirely? (yes — resolved)`;

export const revisions: ProposalHistoryEntry[] = [
  {
    id: "rev-1",
    seq: 1,
    title: "Tribunal chips on the proposals list",
    body: SPEC_V1,
    body_format: "markdown",
    acceptance_criteria: [],
    event_kind: "spec_revision",
    edited_by_user_id: "u-carol",
    created_at: ISO(days(6)),
  },
  {
    id: "rev-2",
    seq: 2,
    title: "Tribunal chips on the proposals list",
    body: SPEC_V2,
    body_format: "markdown",
    acceptance_criteria: [],
    event_kind: "spec_revision",
    edited_by_user_id: "u-alice",
    created_at: ISO(days(3)),
    event_metadata: JSON.stringify({ source: "refinement_loop", round: 1 }),
  },
  {
    id: "rev-3",
    seq: 3,
    title: "Tribunal chips on the proposals list",
    body: SPEC_V3,
    body_format: "markdown",
    acceptance_criteria: [],
    event_kind: "spec_revision",
    edited_by_user_id: "u-alice",
    created_at: ISO(days(2)),
    event_metadata: JSON.stringify({ source: "refinement_loop", round: 2 }),
  },
  {
    id: "rev-status-1",
    seq: 3,
    title: "Tribunal chips on the proposals list",
    body: "",
    body_format: "markdown",
    acceptance_criteria: [],
    event_kind: "status_change",
    edited_by_user_id: "u-bob",
    created_at: ISO(days(1)),
    status_from: "draft",
    status_to: "in_review",
  },
];

// ── Debate trail (objections / rebuttals / verdicts across rounds) ────────────

export const debateTrail: ProposalDebateTrailRow[] = [
  {
    id: "dt-r1-obj-1",
    proposal_id: "prop-refine",
    kind: "objection",
    body: "The spec never says whether the batched summary is computed for terminal proposals. That is a real cost question and must be pinned before build.",
    blocking: true,
    agent_role: "adversary",
    author_kind: "agent",
    author_model: "claude-opus",
    against_revision_seq: 1,
    round: 1,
    resolved_at: ISO(days(3)),
    resolved_by_user_id: null,
    created_at: ISO(days(5)),
    updated_at: ISO(days(3)),
  },
  {
    id: "dt-r1-reb-1",
    proposal_id: "prop-refine",
    kind: "rebuttal",
    body: "Terminal proposals (done/rejected/archived/superseded) return a null summary — the batch skips them entirely. Added to the Open questions section.",
    blocking: false,
    agent_role: "advocate",
    author_kind: "agent",
    author_model: "gpt-5",
    against_revision_seq: 1,
    round: 1,
    created_at: ISO(days(5) - minutes(20)),
    updated_at: ISO(days(5) - minutes(20)),
  },
  {
    id: "dt-r1-verdict",
    proposal_id: "prop-refine",
    kind: "verdict",
    body: "Verdict: needs-work — the terminal-skip is now stated but the query-budget claim is still unverified.",
    blocking: true,
    agent_role: "judge",
    author_kind: "agent",
    author_model: "claude-opus",
    against_revision_seq: 2,
    round: 1,
    created_at: ISO(days(5) - minutes(30)),
    updated_at: ISO(days(5) - minutes(30)),
  },
  {
    id: "dt-r2-obj-1",
    proposal_id: "prop-refine",
    kind: "objection",
    body: "Floating awaiting-review rows to the top changes sort stability. Confirm ties fall back to updated_at desc so rows do not jitter.",
    blocking: false,
    agent_role: "adversary",
    author_kind: "agent",
    author_model: "claude-opus",
    against_revision_seq: 2,
    round: 2,
    created_at: ISO(days(2) - minutes(40)),
    updated_at: ISO(days(2) - minutes(40)),
  },
  {
    id: "dt-r2-verdict",
    proposal_id: "prop-refine",
    kind: "verdict",
    body: "Verdict: approve — the sort tie-break is specified and the query-budget question is resolved. The spec is ready.",
    blocking: false,
    agent_role: "judge",
    author_kind: "agent",
    author_model: "gpt-5",
    against_revision_seq: 3,
    round: 2,
    created_at: ISO(days(2) - minutes(50)),
    updated_at: ISO(days(2) - minutes(50)),
  },
];

const JUDGE_VERDICT_BODY = `**Verdict: approve.**

Over two rounds the tribunal closed the two blocking concerns:

- terminal proposals are excluded from the batched summary, and
- awaiting-review rows sort ahead of the recency order with a stable
  \`updated_at desc\` tie-break.

The refined spec is internally consistent and ready to graduate.`;

// ── Refinement statuses ──────────────────────────────────────────────────────

/** Converged tribunal parked for the human accept/reject review. */
export const refinementAwaitingReview: ProposalRefinementStatus = {
  active: true,
  current_round: 2,
  dry_rounds: 1,
  total_entries: 5,
  awaiting_review: true,
  judge_summary: "The tribunal converged after two rounds; the refined spec resolves both blocking objections.",
  snapshot_revision_seq: 1,
  evidence_lifecycle_state: "active",
};

/** A round still running autonomously (no review yet). */
export const refinementRunning: ProposalRefinementStatus = {
  active: true,
  current_round: 2,
  dry_rounds: 0,
  total_entries: 4,
  awaiting_review: false,
  evidence_lifecycle_state: "active",
};

/** Stopped after the adversary went dry. */
export const refinementStopped: ProposalRefinementStatus = {
  active: false,
  current_round: 3,
  dry_rounds: 3,
  total_entries: 9,
  stop_reason: "adversary_dry",
  evidence_lifecycle_state: "terminal",
};

// ── Gate statuses ────────────────────────────────────────────────────────────

/** DoR partially met, judge needs-work, an unresolved blocking entry. */
export const gateBlocked: ProposalGateStatus = {
  ready: false,
  dor_ready: false,
  dor_failures: [
    {
      check: "acceptance_criteria_count",
      message: "Add at least 2 acceptance criteria (found 0).",
    },
    {
      check: "open_questions_risks_coverage",
      message: "No Open questions / risks section — call out what is still unknown.",
    },
  ],
  judge_verdict_body: "Verdict: needs-work — the query-budget claim is still unverified.",
  judge_verdict_id: "dt-r1-verdict",
  judge_needs_work: true,
  adversary_dry_count: 0,
  unresolved_blocking_count: 1,
  unresolved_blocking_ids: ["dt-r1-verdict"],
  human_override_active: false,
  blocked_explanations: [
    "Definition of Ready not met (2 checks failing).",
    "Latest judge verdict is needs-work.",
    "1 unresolved blocking debate entry.",
  ],
};

/** Everything clears — the gate is ready. */
export const gateReady: ProposalGateStatus = {
  ready: true,
  dor_ready: true,
  dor_failures: [],
  judge_verdict_body: JUDGE_VERDICT_BODY,
  judge_verdict_id: "dt-r2-verdict",
  judge_needs_work: false,
  adversary_dry_count: 1,
  unresolved_blocking_count: 0,
  unresolved_blocking_ids: [],
  human_override_active: false,
  blocked_explanations: [],
};

// ── Feedback thread ──────────────────────────────────────────────────────────

export const feedback: ProposalFeedback[] = [
  {
    id: "fb-1",
    proposal_id: "prop-refine",
    author_kind: "user",
    author_user_id: "u-bob",
    body: "Love the gate dot, but can we make the red only appear once a tribunal has actually engaged? A fresh draft failing DoR should not shout.",
    created_at: ISO(hours(30)),
    updated_at: ISO(hours(30)),
  },
  {
    id: "fb-2",
    proposal_id: "prop-refine",
    author_kind: "ai",
    author_model: "claude-opus",
    body: "The `w-28` tribunal slot may clip the `R{n} running` label at the narrowest breakpoint. Consider truncation or a wider slot.",
    created_at: ISO(hours(26)),
    updated_at: ISO(hours(26)),
  },
  {
    id: "fb-3",
    proposal_id: "prop-refine",
    author_kind: "user",
    author_user_id: "u-carol",
    body: "Typo in the tooltip: \"objection\" should be pluralized when count != 1.",
    created_at: ISO(days(4)),
    updated_at: ISO(days(2)),
    resolved_at: ISO(days(2)),
    resolved_by_user_id: "u-alice",
    resolved_revision_seq: 3,
  },
];

// ── Sign-offs ────────────────────────────────────────────────────────────────

export const signoffsNone: ProposalSignoff[] = [];

export const signoffsPartial: ProposalSignoff[] = [
  {
    created_at: ISO(days(1)),
    kind: "scoped",
    revision_seq: 3,
    stale: false,
    user_id: "u-bob",
  },
];

export const signoffsComplete: ProposalSignoff[] = [
  {
    created_at: ISO(days(1)),
    kind: "scoped",
    revision_seq: 3,
    stale: false,
    user_id: "u-bob",
  },
  {
    created_at: ISO(hours(20)),
    kind: "technical",
    revision_seq: 2,
    stale: true,
    user_id: "u-alice",
  },
];

// ── Graduated epics ──────────────────────────────────────────────────────────

export const epics: ProposalEpic[] = [
  {
    epic_emoji: "📋",
    epic_id: "epic-list",
    epic_short_id: "ab12",
    epic_title: "Tribunal chips + gate dot on the proposals list",
    needs_reconcile: false,
    project_path: "djinnos/djinn",
    status: "in_progress",
    reconciled_at_revision_seq: 3,
  },
  {
    epic_emoji: "🧪",
    epic_id: "epic-budget",
    epic_short_id: "cd34",
    epic_title: "Batched summary query-budget guardrail",
    needs_reconcile: true,
    project_path: "djinnos/catalog",
    status: "todo",
  },
];

// ── Proposal records ─────────────────────────────────────────────────────────

const baseProposal = {
  acceptance_criteria: [
    { criterion: "Every non-terminal list row renders a tribunal chip and a gate dot.", met: true },
    { criterion: "Awaiting-review rows sort ahead of recency order within their group.", met: true },
    { criterion: "Terminal proposals compute no batched summary.", met: false },
  ],
  body: SPEC_V3,
  body_format: "markdown" as const,
  created_at: ISO(days(6)),
  latest_revision_seq: 3,
  pending_reconcile: false,
  updated_at: ISO(days(1)),
};

/** The rich proposal the detail stories center on. */
export const richProposal: Proposal = {
  ...baseProposal,
  id: "prop-refine",
  short_id: "r7x2",
  title: "Tribunal chips on the proposals list",
  status: "in_review",
  author_user_id: "u-carol",
};

/** The full detail bundle for `prop-refine` (in_review, awaiting-review). */
export const richDetail: ProposalDetail = {
  proposal: richProposal,
  targets,
  feedback,
  revisions,
  signoffs: signoffsPartial,
  epics: [],
  debate_trail: debateTrail,
  refinement: refinementAwaitingReview,
  gate_status: gateBlocked,
};

/** An approved proposal that has graduated into epics (kickoff surface). */
export const graduatedDetail: ProposalDetail = {
  proposal: {
    ...baseProposal,
    id: "prop-building",
    short_id: "b9k1",
    title: "Graduated: tribunal chips shipped",
    status: "building",
    author_user_id: "u-carol",
    build_owner_user_id: "u-alice",
  },
  targets,
  feedback: [],
  revisions,
  signoffs: signoffsComplete,
  epics,
  debate_trail: debateTrail,
  refinement: refinementStopped,
  gate_status: gateReady,
};

// ── List rows (with per-row tribunal/gate summaries) ─────────────────────────

type ListRow = ProposalListRow;

const listSummary = (
  over: Partial<ProposalListSummary>,
): ProposalListSummary => ({
  awaiting_review: false,
  current_round: 0,
  dor_ready: false,
  gate_ready: false,
  needs_evidence: false,
  refinement_active: false,
  unresolved_blocking_count: 0,
  ...over,
});

/** Base lean list-row fields shared by all list fixtures. */
const baseListRow = {
  pending_reconcile: false,
  ac_total: 3,
  ac_met: 2,
  created_at: ISO(days(6)),
  updated_at: ISO(days(1)),
};

export const listProposals: ListRow[] = [
  {
    ...baseListRow,
    id: "prop-building",
    short_id: "b9k1",
    title: "Graduated: tribunal chips shipped",
    status: "building",
    author_user_id: "u-carol",
    list_summary: listSummary({ dor_ready: true, gate_ready: true, current_round: 2 }),
    unresolved_feedback_count: 0,
  },
  {
    ...baseListRow,
    id: "prop-approved",
    short_id: "a3p8",
    title: "Per-user spend limits on the dispatch pool",
    status: "approved",
    author_user_id: "u-bob",
    list_summary: listSummary({ dor_ready: true, gate_ready: true }),
    unresolved_feedback_count: 0,
  },
  {
    ...baseListRow,
    id: "prop-refine",
    short_id: "r7x2",
    title: "Tribunal chips on the proposals list",
    status: "in_review",
    author_user_id: "u-carol",
    updated_at: ISO(minutes(30)),
    list_summary: listSummary({ awaiting_review: true, current_round: 2, dor_ready: true }),
    unresolved_feedback_count: 2,
  },
  {
    ...baseListRow,
    id: "prop-running",
    short_id: "d5m0",
    title: "Slack chat → tasks bridge",
    status: "draft",
    author_user_id: "u-carol",
    updated_at: ISO(hours(2)),
    list_summary: listSummary({ refinement_active: true, current_round: 1 }),
    unresolved_feedback_count: 1,
  },
  {
    ...baseListRow,
    id: "prop-blocked",
    short_id: "e1q9",
    title: "Read-only multi-repo fan-out",
    status: "draft",
    author_user_id: "u-bob",
    updated_at: ISO(hours(6)),
    list_summary: listSummary({ current_round: 2, unresolved_blocking_count: 2 }),
    unresolved_feedback_count: 0,
  },
  {
    ...baseListRow,
    id: "prop-evidence",
    short_id: "f8t4",
    title: "Coordinator advisory-lock leadership",
    status: "draft",
    author_user_id: "u-alice",
    updated_at: ISO(hours(9)),
    list_summary: listSummary({ needs_evidence: true, current_round: 1 }),
    unresolved_feedback_count: 3,
  },
  {
    ...baseListRow,
    id: "prop-triage",
    short_id: "g2w7",
    title: "Untriaged: revive the OKF importer",
    status: "triage",
    author_user_id: "u-carol",
    updated_at: ISO(days(4)),
    list_summary: null,
    unresolved_feedback_count: 0,
  },
  {
    ...baseListRow,
    id: "prop-rejected",
    short_id: "h6y3",
    title: "In-house per-repo preapproval gate",
    status: "rejected",
    author_user_id: "u-bob",
    updated_at: ISO(days(9)),
    list_summary: null,
    unresolved_feedback_count: 0,
  },
  {
    ...baseListRow,
    id: "prop-superseded",
    short_id: "j0z5",
    title: "Old draft: Dolt-backed revision ledger",
    status: "superseded",
    author_user_id: "u-carol",
    updated_at: ISO(days(20)),
    list_summary: null,
    unresolved_feedback_count: 0,
  },
];
