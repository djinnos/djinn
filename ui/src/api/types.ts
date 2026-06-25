/**
 * App types derived from MCP generated types.
 *
 * Overrides `owner` (nullable in practice) and adds `project_id`
 * which is stamped client-side.
 */

import type { TaskListOutputSchema, TaskShowOutputSchema, EpicListOutputSchema, ProposalShowOutputSchema } from "./generated/mcp-tools.gen";

export type AcceptanceCriterion = TaskListOutputSchema.AcceptanceCriterionStatus;

export type Project = import("./server").Project;

export type Task = Omit<TaskShowOutputSchema.TaskShowOutput, "owner"> & {
  owner: string | null;
  // Stamped by the desktop app when fetching from a specific project
  project_id?: string | null;
  // URL of the associated pull request (populated when server supports it)
  pr_url?: string | null;
};

export type Epic = Omit<EpicListOutputSchema.EpicModel, "owner"> & {
  owner: string | null;
};

// Global proposals layer (project-independent). `acceptance_criteria` arrives
// as a parsed array from the MCP tools; SSE sends it as a JSON string which the
// SSE handler normalizes to an array before storing.
export type Proposal = ProposalShowOutputSchema.ProposalModel & {
  /** Body format: 'markdown' (legacy default) or 'mdx' (block-aware). */
  body_format?: "markdown" | "mdx" | string | null;
};
export type ProposalFeedback = ProposalShowOutputSchema.ProposalFeedbackModel;
export type ProposalTarget = ProposalShowOutputSchema.ProposalTargetModel;
export type ProposalRevision = ProposalShowOutputSchema.ProposalRevisionModel;
export type ProposalSignoff = ProposalShowOutputSchema.ProposalSignoffModel;
export type ProposalEpic = ProposalShowOutputSchema.ProposalEpicModel;

/**
 * A structured debate-trail row (objection, rebuttal, or verdict).
 * Separate from `ProposalFeedback` (human discussion): debate rows are typed,
 * carry blocking/agent-role metadata, and have a resolution/reopen lifecycle.
 */
export interface ProposalDebateTrailRow {
  id: string;
  proposal_id: string;
  /** `objection` | `rebuttal` | `verdict`. */
  kind: string;
  body: string;
  /** When true, this entry blocks proposal readiness. */
  blocking: boolean;
  /** Agent role (e.g. "advocate", "adversary", "judge"). */
  agent_role: string;
  /** `agent` or `user`. */
  author_kind: string;
  author_user_id?: string | null;
  author_model?: string | null;
  source_task_id?: string | null;
  /** The proposal revision this entry was written against. */
  against_revision_seq: number;
  /** Debate round (1-based). */
  round: number;
  /** When set, the entry has been resolved. `null` while open. */
  resolved_at?: string | null;
  resolved_by_user_id?: string | null;
  /** When set alongside `resolved_at`, the entry was reopened. */
  reopened_at?: string | null;
  reopened_by_user_id?: string | null;
  created_at: string;
  updated_at: string;
}

/**
 * Refinement session status for a proposal. Derived from refinement lifecycle
 * events and debate-trail entries. When `active` is false and `stop_reason`
 * is null, refinement has never been started for this proposal.
 */
export interface ProposalRefinementStatus {
  /** Whether refinement is currently active. */
  active: boolean;
  /** Current debate round (1-based). null when refinement has not started. */
  current_round?: number | null;
  /** Consecutive adversary dry rounds at the end of the trail. */
  dry_rounds: number;
  /** Total debate-trail entries produced so far. */
  total_entries: number;
  /**
   * Update authority mode: `checkpoint` (advocate revisions require approval)
   * or `auto_accept` (revisions applied automatically).
   */
  update_authority: "checkpoint" | "auto_accept" | string;
  /**
   * When set, refinement has stopped.
   * Values: `adversary_dry`, `round_cap`, `spawn_cap`, `repeated_objection`,
   * `agent_failure`. null while still running or not started.
   */
  stop_reason?: string | null;
  /** Count of pending checkpoint revisions awaiting approval. Always 0 in auto-accept mode. */
  pending_checkpoint_count?: number;
}

/**
 * A pending checkpoint revision visible for approval or rejection.
 * Produced by the Advocate in checkpoint mode; the live proposal body
 * is not mutated until the revision is approved.
 */
export interface CheckpointRevision {
  /** Revision sequence number. */
  seq: number;
  /** Advocate role attribution. */
  role?: string | null;
  /** Refinement round that produced this revision. */
  round?: number | null;
  /** Model that authored this revision. */
  author_model?: string | null;
  /** Short preview of the proposed body (first 300 chars). */
  body_preview: string;
  /** Title of the pending revision. */
  title: string;
  /** When the revision was created. */
  created_at: string;
}

/**
 * One DoR failure in the gate status.
 */
export interface GateFailure {
  /** Which high-level check failed (e.g. `problem_coverage`, `vague_acceptance_criteria`). */
  check: string;
  /** Human-readable failure message. */
  message: string;
}

/**
 * Needs-evidence parking state for a proposal.
 */
export interface NeedsEvidenceStatus {
  /** The named feasibility claim that the Judge identified. */
  claim: string;
  /** The spike task id (UUID). */
  spike_task_id: string;
  /** The spike task short id (human-readable). */
  spike_short_id: string;
  /** Current status of the spike task. */
  spike_status: string;
}

/**
 * Composed gate status for a proposal: deterministic DoR + tribunal
 * conditions. Returned by `proposal_show` so the UI can render readiness
 * without recomputing it client-side.
 */
export interface ProposalGateStatus {
  /** Whether the composed gate passes (DoR ready + tribunal conditions met). */
  ready: boolean;
  /** Whether the deterministic DoR checks pass. */
  dor_ready: boolean;
  /** Specific DoR failures (empty when dor_ready is true). */
  dor_failures: GateFailure[];
  /** Latest judge verdict body text, when a judge has issued a verdict. */
  judge_verdict_body?: string | null;
  /** Latest judge verdict entry id, when a judge has issued a verdict. */
  judge_verdict_id?: string | null;
  /** Whether the latest judge verdict contains "needs-work". */
  judge_needs_work: boolean;
  /** Consecutive adversary dry rounds at the end of the trail. */
  adversary_dry_count: number;
  /** Count of unresolved blocking debate-trail entries. */
  unresolved_blocking_count: number;
  /** IDs of unresolved blocking debate-trail entries. */
  unresolved_blocking_ids: string[];
  /** Needs-evidence spike parking state. null when not parked. */
  needs_evidence?: NeedsEvidenceStatus | null;
  /** Whether there are pending checkpoint revisions awaiting decision. */
  pending_checkpoint: boolean;
  /** Whether a current human override exists for this revision. */
  human_override_active: boolean;
  /** Human-readable explanations of all gate failures. Empty when ready is true. */
  blocked_explanations: string[];
}
