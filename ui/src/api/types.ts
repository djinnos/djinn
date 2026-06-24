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
