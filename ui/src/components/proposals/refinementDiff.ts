import type { ProposalHistoryEntry } from "@/lib/proposalQueries";

/**
 * Compute the pre-refinement snapshot → head spec bodies for the tribunal
 * review diff. Mirrors {@link ProposalHistory}'s refinement-run diff (base
 * body → converged head body) but keyed off the refinement session's recorded
 * `snapshot_revision_seq` rather than a contiguous run of `refinement_loop`
 * revisions, so it stays correct even after the run is collapsed/accepted.
 *
 * Returns `null` when there are no spec revisions to diff.
 */
export function refinementDiffBodies(
  revisions: ProposalHistoryEntry[],
  snapshotSeq: number | null | undefined,
): {
  before: string;
  after: string;
  fromSeq: number | null;
  headSeq: number;
} | null {
  const specs = revisions.filter((r) => r.event_kind === "spec_revision");
  if (specs.length === 0) return null;

  const head = specs.reduce((a, b) => (b.seq > a.seq ? b : a));
  // Base = the snapshot the tribunal ran against, else the revision immediately
  // before the head (a sensible fallback for older payloads without a snapshot).
  const base =
    (snapshotSeq != null
      ? specs.find((r) => r.seq === snapshotSeq)
      : undefined) ?? specs.find((r) => r.seq === head.seq - 1);

  return {
    before: base?.body ?? "",
    after: head.body ?? "",
    fromSeq: base?.seq ?? null,
    headSeq: head.seq,
  };
}
