import { useId } from "react";
import { DistributeHorizontalCenterIcon } from "@hugeicons/core-free-icons";

import { cn } from "@/lib/utils";

import { ProposalBlocks } from "./ProposalBlocks";
import { ContainerFallback } from "./ContainerFallback";
import { parseColumnsAttr } from "./containerAttr";
import type { BlockProps } from "./types";

/**
 * Responsive Tailwind grid classes keyed by column count. Every layout collapses
 * to a single stacked column on narrow screens and fans out to the column count
 * at `md`+, so narrow viewports stack the panels instead of crushing them. Five
 * or more columns reuse the 4-column class (wrapping into multiple rows).
 */
const COLS_CLASS: Record<number, string> = {
  1: "grid-cols-1",
  2: "grid-cols-1 md:grid-cols-2",
  3: "grid-cols-1 md:grid-cols-3",
  4: "grid-cols-1 md:grid-cols-4",
};

/** Resolve the responsive grid class for a column count (clamped to 1–4). */
function gridColsClass(count: number): string {
  return COLS_CLASS[Math.min(4, Math.max(1, count))] ?? COLS_CLASS[1];
}

/**
 * Columns — a recursive container block. The `columns` attribute carries a JSON
 * array (`columns={[{ "body" }, ...]}`) where each entry's `body` is a normal
 * proposal-MDX string rendered RECURSIVELY through the shared
 * {@link ProposalBlocks} renderer. Columns sit side by side on wide screens and
 * stack on narrow ones.
 *
 * If the `columns` attribute is missing or not valid JSON the block renders a
 * calm fallback (its children markdown, else a muted notice plus the raw
 * attribute) — it never throws or blanks.
 */
export function ColumnsBlock({ attributes, children }: BlockProps) {
  const columns = parseColumnsAttr(attributes.columns);
  const baseId = useId();

  if (!columns) {
    return (
      <ContainerFallback
        kind="columns"
        rawAttribute={attributes.columns}
        icon={DistributeHorizontalCenterIcon}
      >
        {children}
      </ContainerFallback>
    );
  }

  return (
    <div className={cn("grid gap-4", gridColsClass(columns.length))}>
      {columns.map((column, index) => (
        <div key={`${baseId}-col-${index}`} className="min-w-0">
          <ProposalBlocks body={column.body} />
        </div>
      ))}
    </div>
  );
}
