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
  body_format?: "markdown" | "mdx" | string;
};
export type ProposalFeedback = ProposalShowOutputSchema.ProposalFeedbackModel;
export type ProposalTarget = ProposalShowOutputSchema.ProposalTargetModel;
export type ProposalRevision = ProposalShowOutputSchema.ProposalRevisionModel;
export type ProposalSignoff = ProposalShowOutputSchema.ProposalSignoffModel;
export type ProposalEpic = ProposalShowOutputSchema.ProposalEpicModel;
