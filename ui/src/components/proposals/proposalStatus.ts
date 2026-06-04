export type ProposalStatus =
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
  in_review: { label: "In Review", order: 0, dot: "bg-amber-500" },
  draft: { label: "Draft", order: 1, dot: "border-2 border-dashed border-muted-foreground/50" },
  approved: { label: "Approved", order: 2, dot: "bg-blue-500" },
  building: { label: "Building", order: 3, dot: "bg-purple-500" },
  done: { label: "Done", order: 4, dot: "bg-green-500", done: true, terminal: true },
  rejected: { label: "Rejected", order: 5, dot: "bg-red-500/70", terminal: true },
  superseded: { label: "Superseded", order: 6, dot: "bg-muted-foreground/50", terminal: true },
  archived: { label: "Archived", order: 7, dot: "bg-muted-foreground/40", terminal: true },
};

export const PROPOSAL_STATUS_KEYS = (
  Object.keys(PROPOSAL_STATUS_META) as ProposalStatus[]
).sort((a, b) => PROPOSAL_STATUS_META[a].order - PROPOSAL_STATUS_META[b].order);

export function statusLabel(status: string): string {
  return PROPOSAL_STATUS_META[status as ProposalStatus]?.label ?? status;
}

export function isArchivedLike(status: string): boolean {
  return status === "archived" || status === "superseded";
}
