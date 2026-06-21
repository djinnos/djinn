import { useMemo } from "react";

import { BlockMarkdown, BlockShell } from "./BlockShell";
import { parseTable } from "./table";
import type { BlockProps } from "./types";

/**
 * Table — a clean styled grid parsed from a GFM markdown table (or bare pipe
 * rows) authored as the block's children text. The first row is the header
 * (subtle bottom border, semibold), body rows are separated by faint dividers,
 * and each cell renders inline markdown. The surface is horizontally scrollable
 * and pinned LTR so wide tables behave in any prose direction.
 *
 * Ported from agent-native's `table` block. If the children don't parse as a
 * table, we fall back to `BlockMarkdown` so an existing proposal never blanks.
 * Dark-only.
 */
export function TableBlock({ children }: BlockProps) {
  const body = typeof children === "string" ? children : "";
  const table = useMemo(() => parseTable(body), [body]);

  // Not a table — render the raw children as markdown.
  if (!table) {
    return (
      <BlockShell label="Table" accent="text-slate-400">
        <BlockMarkdown>{body}</BlockMarkdown>
      </BlockShell>
    );
  }

  return (
    <BlockShell label="Table" accent="text-slate-400" flush>
      <div className="overflow-x-auto" dir="ltr">
        <table className="w-full border-collapse text-left text-sm">
          <thead>
            <tr className="border-b border-border bg-muted/30">
              {table.header.map((cell, index) => (
                <th
                  key={index}
                  className="px-4 py-2.5 align-top font-semibold text-foreground"
                >
                  <CellMarkdown>{cell}</CellMarkdown>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {table.rows.map((row, rowIndex) => (
              <tr
                key={rowIndex}
                className="border-b border-border/50 last:border-b-0"
              >
                {row.map((cell, cellIndex) => (
                  <td
                    key={cellIndex}
                    className="px-4 py-2.5 align-top text-muted-foreground"
                  >
                    <CellMarkdown mono={cellIndex === 0}>{cell}</CellMarkdown>
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </BlockShell>
  );
}

/**
 * A single table cell. Empty cells render a muted em-dash; cells with inline
 * markdown go through `BlockMarkdown`; the first body column gets a mono face so
 * identifiers/keys read cleanly.
 */
function CellMarkdown({
  children,
  mono,
}: {
  children: string;
  mono?: boolean;
}) {
  if (children.trim() === "") {
    return <span className="text-muted-foreground/40">—</span>;
  }
  return (
    <div className={mono ? "font-mono text-[13px]" : undefined}>
      <BlockMarkdown>{children}</BlockMarkdown>
    </div>
  );
}
