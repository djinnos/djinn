export type ProposalStatus =
  | "triage"
  | "draft"
  | "in_review"
  | "approved"
  | "building"
  | "done"
  | "rejected"
  | "archived"
  | "superseded";

export interface StatusMeta {
  label: string;
  /** Lifecycle order for grouping (lower = earlier / higher in the list). */
  order: number;
  /** Tailwind classes for the colored dot/ring. */
  dot: string;
  /** Render a check glyph inside (terminal success). */
  done?: boolean;
  /** Off-ramp / closed state — collapsed/hidden together. */
  terminal?: boolean;
}

export const PROPOSAL_STATUS_META: Record<ProposalStatus, StatusMeta> = {
  triage: { label: "Triage", order: 0, dot: "border-2 border-dashed border-muted-foreground/50" },
  draft: { label: "Draft", order: 1, dot: "bg-amber-500/70" },
  in_review: { label: "In Review", order: 2, dot: "bg-amber-500" },
  approved: { label: "Approved", order: 3, dot: "bg-blue-500" },
  building: { label: "Building", order: 4, dot: "bg-purple-500" },
  done: { label: "Done", order: 5, dot: "bg-green-500", done: true, terminal: true },
  rejected: { label: "Rejected", order: 6, dot: "bg-red-500/70", terminal: true },
  superseded: { label: "Superseded", order: 7, dot: "bg-muted-foreground/50", terminal: true },
  archived: { label: "Archived", order: 8, dot: "bg-muted-foreground/40", terminal: true },
};

// Display order, most-advanced first: the groups at the top are the ones that
// have moved furthest down the lifecycle. Off-ramp / dead states (rejected,
// superseded, archived) stay pinned at the bottom rather than above active work.
export const PROPOSAL_STATUS_KEYS: ProposalStatus[] = [
  "done",
  "building",
  "approved",
  "in_review",
  "draft",
  "triage",
  "rejected",
  "superseded",
  "archived",
];

export function statusLabel(status: string): string {
  return PROPOSAL_STATUS_META[status as ProposalStatus]?.label ?? status;
}

export function isArchivedLike(status: string): boolean {
  return status === "archived" || status === "superseded";
}
