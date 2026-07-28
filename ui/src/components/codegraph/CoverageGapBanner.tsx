/**
 * CoverageGapBanner (proposal glqk) — the galaxy HUD's index-coverage advisory.
 *
 * The code graph is best-effort: SCIP indexers fail per-workspace and the warm
 * succeeds with whatever remains. This chip names the workspaces whose index is
 * NOT current so a viewer never reads a silent false negative (dead code / no
 * callers / impact) off the galaxy. Renders nothing when coverage is clean.
 *
 * All the phrasing decisions — cause, fallback, staleness, retriability — live
 * in `./coverageAdvisory`, which documents why a gap is usually "stale" rather
 * than "not indexed". This file is presentation only.
 *
 * Chip styling voice mirrors GalaxyView's HUD_CHIP, in the amber the canvas
 * already uses for its "showing N of M" honesty span.
 */

import type { CodeGraphCoverage, CodeGraphWorkspace } from "@/api/codeGraph";
import {
  fallbackFor,
  summarize,
  summarizeMany,
  tooltipFor,
} from "./coverageAdvisory";

/** Same chip voice as GalaxyView's HUD_CHIP, tinted amber for the advisory. */
const COVERAGE_CHIP =
  "flex items-center gap-1.5 rounded-lg border border-amber-500/40 bg-amber-950/40 px-2.5 py-1 font-mono text-[11px] text-amber-300 backdrop-blur-sm";

export function CoverageGapBanner({
  coverage,
  workspaces = [],
  now,
}: {
  coverage: CodeGraphCoverage | null;
  /** From the `workspaces` op — supplies the fallback (node count + commit)
   * that separates "stale" from "missing". Omitted, every gap reads as
   * missing, which is the pre-existing, more alarming behaviour. */
  workspaces?: CodeGraphWorkspace[];
  /** Wall clock at fetch time, supplied by the caller so this component stays
   * pure and the age does not silently tick between re-renders. Omitted, the
   * "N ago" clause is dropped rather than guessed. */
  now?: number;
}) {
  if (!coverage || !coverage.hasGaps || coverage.gaps.length === 0) {
    return null;
  }

  const fallbacks = coverage.gaps.map((g) => fallbackFor(g.slug, workspaces));
  // One gap gets the full sentence; several get a list, because the chip is a
  // single HUD line and the detail lives in the tooltip either way.
  const text =
    coverage.gaps.length === 1
      ? summarize(coverage.gaps[0], fallbacks[0], now)
      : summarizeMany(coverage.gaps, fallbacks);

  return (
    <div
      className={COVERAGE_CHIP}
      role="status"
      aria-label="Index coverage gap"
      title={tooltipFor(coverage.gaps, fallbacks)}
    >
      <span aria-hidden="true">⚠</span>
      <span className="text-amber-200/90">{text}</span>
    </div>
  );
}
