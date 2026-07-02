import type { ProposalDebateTrailRow } from "@/api/types";

/** Parsed verdict outcome for a `verdict` debate-trail row. */
export interface VerdictOutcome {
  label: string;
  positive: boolean;
}

/**
 * Derive a verdict's outcome. Prefers a leading `Verdict: approve/reject/…`
 * marker in the body; falls back to the `blocking` flag (a blocking verdict
 * reads as reject/needs-work, a non-blocking one as approve).
 */
export function verdictOutcome(row: ProposalDebateTrailRow): VerdictOutcome {
  const match = row.body.match(/verdict:?\s*([a-z][a-z -]*)/i);
  if (match) {
    const word = match[1].trim().toLowerCase();
    if (/^(approve|approved|ready|pass|passed|accept)/.test(word)) {
      return { label: word, positive: true };
    }
    if (/^(reject|rejected|needs?[ -]?work|block|blocked|fail)/.test(word)) {
      return { label: word, positive: false };
    }
    return { label: word, positive: !row.blocking };
  }
  return {
    label: row.blocking ? "needs-work" : "approve",
    positive: !row.blocking,
  };
}
