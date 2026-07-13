/**
 * App types derived from MCP generated types.
 *
 * Overrides `owner` (nullable in practice) and adds `project_id`
 * which is stamped client-side.
 */

import type { TaskListOutputSchema, TaskShowOutputSchema, EpicListOutputSchema, ProposalShowOutputSchema } from "./generated/mcp-tools.gen";

export type AcceptanceCriterion = TaskListOutputSchema.AcceptanceCriterionStatus;

export type Project = import("./server").Project;

/**
 * CI status values exposed by the backend CiStatus enum.
 * Wire values are lowercase snake-case strings.
 */
export type CiStatus = "passing" | "failing" | "pending" | "unknown";

/**
 * Structured CI gate snapshot from the backend.
 * Rendered from the task's `ci` field when a PR snapshot exists.
 */
export interface CiGateSnapshot {
  /** Current required-CI status for the PR head. */
  status: CiStatus;
  /** Derived display/gate state. `awaiting_ci` appears for pr_draft pending/unknown snapshots. */
  gate_state?: CiStatus | "awaiting_ci";
  /** Git SHA of the PR head this snapshot describes. */
  head_sha: string;
  /** Names of required checks that are currently failing and blocking merge. */
  blocking_required_check_names: string[];
  /** Primary blocking required check name, when CI is failing. */
  primary_blocking_check?: string | null;
  /** Stable fingerprint of the current failure signature. */
  failure_fingerprint?: string | null;
  /** Human-readable summary derived from the structured CI gate snapshot. */
  summary_reason?: string | null;
  /** Structured merge/close blocking reason when required CI is not passing. */
  merge_blocked_reason?: string | null;
  /** ISO-8601 timestamp when this snapshot was first observed. */
  first_seen_at: string;
  /** ISO-8601 timestamp when this snapshot was last observed/updated. */
  last_seen_at: string;
  /** How many consecutive observations carried the same failure fingerprint. */
  same_signature_count: number;
  /** Base SHA of the last remediation attempt. */
  last_remediation_base_sha?: string | null;
  /** GitHub PR number, when available. */
  pr_number?: number | null;
}

export type Task = Omit<TaskShowOutputSchema.TaskShowOutput, "owner"> & {
  owner: string | null;
  // Stamped by the desktop app when fetching from a specific project
  project_id?: string | null;
  // URL of the associated pull request (populated when server supports it)
  pr_url?: string | null;
  // CI gate snapshot for this task's PR, if any
  ci?: CiGateSnapshot | null;
};

export type Epic = Omit<EpicListOutputSchema.EpicModel, "owner"> & {
  owner: string | null;
  /** Originating proposal id (denormalized at graduation). */
  proposal_id?: string | null;
  /** Proposal short id/title/status — enriched on `epic_list` responses so the
   * board can label proposal swimlanes; absent on SSE epic payloads. */
  proposal_short_id?: string | null;
  proposal_title?: string | null;
  proposal_status?: string | null;
  proposal_build_owner_user_id?: string | null;
};

// Global proposals layer (project-independent). `acceptance_criteria` arrives
// as a parsed array from the MCP tools; SSE sends it as a JSON string which the
// SSE handler normalizes to an array before storing.
export type Proposal = ProposalShowOutputSchema.ProposalModel & {
  /** Body format: 'markdown' (legacy default) or 'mdx' (block-aware). */
  body_format?: "markdown" | "mdx" | string | null;
};
/**
 * Compact tribunal/readiness summary for a proposal list row. Present only on
 * `proposal_list` (batched across the page) for non-terminal proposals; drives
 * the list-row tribunal/gate chips.
 */
export type ProposalListSummary = ProposalShowOutputSchema.ProposalListSummary;
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
   * When set, refinement has stopped.
   * Values: `adversary_dry`, `round_cap`, `spawn_cap`, `repeated_objection`,
   * `agent_failure`. null while still running or not started.
   */
  stop_reason?: string | null;
  /**
   * True when the autonomous tribunal has converged and parked for the
   * single human accept/reject review of the full refined result.
   */
  awaiting_review?: boolean;
  /** The judge's summary of the refined result, shown at review time. */
  judge_summary?: string | null;
  /** Pre-refinement baseline revision sequence (diff baseline). */
  snapshot_revision_seq?: number | null;
  /**
   * Top-level evidence lifecycle state derived from durable proposal
   * fields, lifecycle events, and linked-spike task status. Lets the UI
   * distinguish Active, AwaitingEvidence, EvidenceFailed, PausedOrFrozen,
   * and Terminal without inspecting individual sub-fields.
   */
  evidence_lifecycle_state?:
    | "active"
    | "awaiting_evidence"
    | "evidence_received"
    | "evidence_failed"
    | "paused_or_frozen"
    | "terminal";
  /**
   * When the proposal is parked for a needs-evidence spike, this contains
   * the claim and spike task reference. `null` when not parked.
   */
  needs_evidence?: NeedsEvidenceStatus | null;
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
  /**
   * Current evidence lifecycle phase (awaiting, received, or failed).
   * Derived from persisted `proposal_revisions` lifecycle events.
   * `null` when no lifecycle event has been recorded yet.
   */
  evidence_phase?: "awaiting_evidence" | "evidence_received" | "evidence_failed" | null;
  /**
   * For `evidence_failed`, the failure reason (`spike_cancelled`,
   * `spike_errored`, `spike_force_closed`, `malformed_findings`, etc.).
   * `null` for other phases.
   */
  failure_reason?: string | null;
  /**
   * The feasibility question the spike must answer (from the structured
   * claim). `null` when `claim` is a legacy plain string.
   */
  question?: string | null;
  /** Debate round when the demand was issued. */
  round?: number | null;
  /** What in the spec is unknown/unverified. */
  spec_unknown_anchor?: string | null;
  /** The subsystem or module under investigation. */
  target_subsystem?: string | null;
  /** Proposal revision sequence the demand targets. */
  against_revision_seq?: number | null;
  /** The Judge task id that issued the demand. */
  created_by_task_id?: string | null;
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
  /** Whether a current human override exists for this revision. */
  human_override_active: boolean;
  /** Human-readable explanations of all gate failures. Empty when ready is true. */
  blocked_explanations: string[];
}
