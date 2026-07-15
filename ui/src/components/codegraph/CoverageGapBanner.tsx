/**
 * CoverageGapBanner (proposal glqk) — the galaxy HUD's index-coverage advisory.
 *
 * The code graph is best-effort: SCIP indexers fail per-workspace and the warm
 * succeeds with whatever remains. This chip names the workspaces that are NOT
 * indexed so a viewer never reads a silent false negative (dead code / no
 * callers / impact) off the galaxy. Renders nothing when coverage is clean.
 *
 * Chip styling voice mirrors GalaxyView's HUD_CHIP, in the amber the canvas
 * already uses for its "showing N of M" honesty span.
 */

import type { CodeGraphCoverage } from "@/api/codeGraph";

/** Same chip voice as GalaxyView's HUD_CHIP, tinted amber for the advisory. */
const COVERAGE_CHIP =
  "flex items-center gap-1.5 rounded-lg border border-amber-500/40 bg-amber-950/40 px-2.5 py-1 font-mono text-[11px] text-amber-300 backdrop-blur-sm";

export function CoverageGapBanner({
  coverage,
}: {
  coverage: CodeGraphCoverage | null;
}) {
  if (!coverage || !coverage.hasGaps || coverage.gaps.length === 0) {
    return null;
  }

  const names = coverage.gaps
    .map((g) => `${g.slug} (${g.language})`)
    .join(", ");
  const count = coverage.gaps.length;

  return (
    <div
      className={COVERAGE_CHIP}
      role="status"
      aria-label="Index coverage gap"
      title={
        "These workspaces are not indexed — dead-code / no-callers / impact " +
        "results may be false negatives. Verify with grep before trusting an absence."
      }
    >
      <span aria-hidden="true">⚠</span>
      <span className="font-semibold">
        {count} workspace{count === 1 ? "" : "s"} not indexed:
      </span>
      <span className="text-amber-200/90">{names}</span>
    </div>
  );
}
