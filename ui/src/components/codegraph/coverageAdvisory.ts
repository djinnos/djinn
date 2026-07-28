/**
 * Pure phrasing logic behind [`CoverageGapBanner`] (proposal glqk).
 *
 * # Why this says more than "not indexed"
 *
 * The semantic index is no longer a step that either ran or didn't. It is a
 * standalone, content-keyed, retried pipeline, and a gap has two independent
 * axes the old wording collapsed into one scary word:
 *
 * * **cause** — `timed_out` is a budget that was too small and grows itself on
 *   the next attempt; `indexer_failed` is a real error; `unsupported_language`
 *   will never resolve on its own. Only the last is permanent.
 * * **fallback** — a failed attempt does NOT empty the workspace. The warm
 *   salvages the previous artifact, so the galaxy usually still shows that
 *   workspace's symbols, just pinned to an older commit. "Not indexed" claims
 *   an absence that is generally false; "stale" is the truth, and it is a much
 *   weaker warning.
 *
 * Both axes are derivable from data the HUD already fetches: `coverage` gives
 * cause + extent + attempt time, and `workspaces` gives the node count and the
 * commit those nodes came from. A workspace carrying nodes at a commit behind
 * the rest of the graph is stale, not missing.
 *
 * Lives outside the component file so the component module exports only a
 * component (react-refresh) and so this logic is unit-testable without a DOM.
 */

import type { CodeGraphCoverage, CodeGraphWorkspace } from "@/api/codeGraph";

type Gap = CodeGraphCoverage["gaps"][number];

/** Short human cause per coverage status. Unknown statuses stay generic rather
 * than asserting a cause the server never claimed. */
export function causeLabel(status: string): string {
  switch (status) {
    case "timed_out":
      return "index timed out";
    case "indexer_failed":
      return "indexer failed";
    case "unsupported_language":
      return "no indexer";
    default:
      return "not indexed";
  }
}

/** `true` when the status resolves itself on a later attempt. A timeout grows
 * its own budget; a crash gets retried. A missing indexer never will. */
export function retriable(status: string): boolean {
  return status !== "unsupported_language";
}

/** Compact relative age, e.g. `30s`, `30m`, `2h`, `3d`. Returns undefined for an
 * absent/unparseable timestamp OR an unknown `now`, so callers omit the clause
 * entirely instead of rendering an invented age. */
export function relativeAge(
  iso: string | undefined,
  now: number | undefined,
): string | undefined {
  if (!iso || now === undefined) return undefined;
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return undefined;
  const secs = Math.max(0, Math.round((now - then) / 1000));
  if (secs < 90) return `${secs}s`;
  const mins = Math.round(secs / 60);
  if (mins < 90) return `${mins}m`;
  const hours = Math.round(mins / 60);
  if (hours < 36) return `${hours}h`;
  return `${Math.round(hours / 24)}d`;
}

/** What the galaxy is actually showing for a gap workspace right now. */
export type Fallback =
  | { kind: "stale"; commit: string; nodes: number }
  | { kind: "present"; nodes: number }
  | { kind: "missing" };

/**
 * Decide what the viewer is looking at for this workspace.
 *
 * `stale` requires positive evidence on both legs: the workspace contributes
 * nodes AND its commit differs from the commit the rest of the graph settled
 * on. Without a comparable peer commit we say `present` — we know there is
 * data, we cannot prove it is behind — rather than guessing either way.
 */
export function fallbackFor(
  slug: string,
  workspaces: CodeGraphWorkspace[],
): Fallback {
  const row = workspaces.find((w) => w.slug === slug);
  const nodes = row?.nodeCount ?? 0;
  if (!row || nodes <= 0) return { kind: "missing" };
  // The graph's commit is the one the healthy workspaces agree on. Mode, not
  // max: shas do not order, and the gap workspace itself must not vote.
  const tally = new Map<string, number>();
  for (const w of workspaces) {
    if (w.slug === slug || !w.commitSha) continue;
    tally.set(w.commitSha, (tally.get(w.commitSha) ?? 0) + 1);
  }
  let graphCommit: string | undefined;
  let best = 0;
  for (const [sha, count] of tally) {
    if (count > best) {
      best = count;
      graphCommit = sha;
    }
  }
  if (row.commitSha && graphCommit && row.commitSha !== graphCommit) {
    return { kind: "stale", commit: row.commitSha.slice(0, 7), nodes };
  }
  return { kind: "present", nodes };
}

/** The one-line chip text for a single gap. */
export function summarize(
  gap: Gap,
  fallback: Fallback,
  now: number | undefined,
): string {
  const age = relativeAge(gap.attemptedAt, now);
  const cause = `${causeLabel(gap.status)}${age ? ` ${age} ago` : ""}`;
  switch (fallback.kind) {
    case "stale":
      return `${gap.slug} (${gap.language}): ${cause} — showing ${fallback.commit}`;
    case "present":
      return `${gap.slug} (${gap.language}): ${cause} — showing last good index`;
    case "missing":
      return `${gap.slug} (${gap.language}): ${cause} — not in the graph`;
  }
}

/** The compact multi-gap list: one clause per workspace, cause + fallback. */
export function summarizeMany(gaps: Gap[], fallbacks: Fallback[]): string {
  const parts = gaps.map(
    (g, i) =>
      `${g.slug} (${g.language}, ${causeLabel(g.status)}${
        fallbacks[i].kind === "missing" ? ", not in graph" : ", stale"
      })`,
  );
  return `${gaps.length} workspaces degraded: ${parts.join(", ")}`;
}

/** The hover explanation: what is wrong, what you are seeing, what happens next. */
export function tooltipFor(gaps: Gap[], fallbacks: Fallback[]): string {
  const anyFallback = fallbacks.some((f) => f.kind !== "missing");
  const anyRetriable = gaps.some((g) => retriable(g.status));
  const extent = gaps
    .filter((g) => g.discoveredFiles !== undefined)
    .map((g) => `${g.slug}: 0 of ${g.discoveredFiles} files re-indexed`)
    .join("; ");
  return [
    anyFallback
      ? "The last index attempt for these workspaces did not complete, so the galaxy is serving the previous index for them. Symbols added or moved since that commit are missing — dead-code / no-callers / impact results can be false negatives. Verify with grep before trusting an absence."
      : "These workspaces contribute nothing to the graph — dead-code / no-callers / impact results for them are false negatives by construction. Verify with grep before trusting an absence.",
    extent,
    anyRetriable
      ? "The semantic indexer re-attempts on its own schedule and grows a timed-out workspace's budget each round; no action is needed unless this persists."
      : "This will not resolve on its own — no indexer exists for that language.",
  ]
    .filter(Boolean)
    .join("\n\n");
}
