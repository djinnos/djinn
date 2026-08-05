/**
 * View model for the session ledger.
 *
 * The live thread renders a flat list of messages; the ledger renders the
 * structure that list implies — PHASE > TURN > (activity | say | artifact),
 * with HANDOFF between phases. Keeping the model separate from
 * `useSessionMessages` types lets the presentational component be driven by
 * fixtures while the adapter that derives these from a real timeline is
 * written against it later.
 */

/** One step inside an activity strand: reasoning or a tool call, in real order. */
export interface ActivityStep {
  kind: "thinking" | "tool";
  /** "Reviewing diff names" for thinking; the tool name ("Read", "Bash") for tools. */
  label: string;
  /** Argument preview for tools; nothing for thinking. */
  detail?: string;
  /** Right-aligned annotation: "0.4s", "9 hits". */
  meta?: string;
}

/**
 * Reasoning and tool calls merged into one chronological run. Collapsed to a
 * single summary line by default — this is what replaces the wall of chips.
 */
export interface ActivityStrand {
  kind: "activity";
  steps: ActivityStep[];
  /** "6m04s" */
  durationLabel?: string;
  /** Still executing: renders the live dot and the `now →` line. */
  running?: boolean;
  /** What the agent is doing right now, shown while running. */
  nowLabel?: string;
}

/** Prose the agent addressed to a human. */
export interface SayBlock {
  kind: "say";
  markdown: string;
}

export type ArtifactVariant =
  | "work_submitted"
  | "review_submitted"
  | "lead_decision"
  | "grooming"
  | "escalated";

/** A terminal submission — the load-bearing event of a phase. */
export interface ArtifactBlock {
  kind: "artifact";
  variant: ArtifactVariant;
  summary: string;
  /** Collapsed behind a count; these bury the summary when listed inline. */
  files?: string[];
  concerns?: string[];
  /** Review verdict / decision outcome, shown as a pill. */
  outcome?: string;
  timestamp: string;
}

export type TurnBlock = ActivityStrand | SayBlock | ArtifactBlock;

/** One agent, one loop iteration. Carries a single avatar regardless of block count. */
export interface LedgerTurn {
  id: string;
  agentType: string;
  blocks: TurnBlock[];
}

export interface LedgerBrief {
  body: string;
  filedBy: string;
  timestamp: string;
  /** "Design", "Source commits (4)" — disclosure affordances, not content. */
  facets?: { label: string; count?: number }[];
}

export interface LedgerPhase {
  kind: "phase";
  id: string;
  /** "IMPLEMENTATION", "REVIEW", "LEAD" — or "BRIEF" for the origin band. */
  title: string;
  agentType?: string;
  modelId?: string;
  durationLabel?: string;
  running?: boolean;
  /**
   * Set when a phase spanned more than one session. A phase whose agent died
   * and respawned renders as one phase with N attempts; without this the dead
   * sessions render as nothing at all.
   */
  attempts?: { total: number; failed: number };
  turns: LedgerTurn[];
  /** Present only on the origin band. */
  brief?: LedgerBrief;
}

export interface LedgerHandoff {
  kind: "handoff";
  id: string;
  /** Absent on the first dispatch. */
  from?: string;
  to: string;
  /** "HANDOFF" | "DISPATCHED" | "ESCALATED" */
  label: string;
  timestamp: string;
}

export type LedgerEntry = LedgerPhase | LedgerHandoff;

export interface AcceptanceCriterion {
  text: string;
  met: boolean;
  /** "19:41" */
  metAt?: string;
  /** Why an unmet criterion is unmet — the thing worth seeing at a glance. */
  note?: string;
}

export interface LedgerAgentStatus {
  agentType: string;
  durationLabel: string;
  status: string;
  running?: boolean;
}

/** The sticky footer state: answers "is anything happening" without scrolling. */
export interface LedgerLiveState {
  agentType: string;
  durationLabel: string;
  stepLabel: string;
  nowLabel: string;
}

export interface SessionLedgerProps {
  taskShortId: string;
  taskTitle: string;
  statusLabel: string;
  usageLabel?: string;
  criteria: AcceptanceCriterion[];
  agents: LedgerAgentStatus[];
  blockers?: string[];
  entries: LedgerEntry[];
  live?: LedgerLiveState | null;
  /** Off when the host page already renders its own title bar. */
  showHeader?: boolean;
  /** Rendered in place of the thread when there is nothing to show. */
  emptyMessage?: string;
}
