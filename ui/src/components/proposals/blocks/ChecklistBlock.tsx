import { useMemo } from "react";
import { CheckmarkSquare02Icon, SquareIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { cn } from "@/lib/utils";

import { BlockMarkdown, BlockShell } from "./BlockShell";
import { parseChecklist } from "./checklist";
import type { BlockProps } from "./types";

/**
 * Checklist — a read-only list of items with a check/empty box reflecting the
 * AUTHORED state (`- [x]` / `- [ ]`), the item text, and an optional muted note.
 * A small "N/M done" tally sits in the header.
 *
 * Ported from agent-native's `checklist` block, but READ-ONLY: the boxes are NOT
 * toggleable by viewers (no submit / state capture), matching djinn's no-form
 * product stance. If nothing parses as a list we fall back to `BlockMarkdown` so
 * an existing proposal never blanks. Dark-only.
 */
export function ChecklistBlock({ children }: BlockProps) {
  const body = typeof children === "string" ? children : "";
  const { items, done, total } = useMemo(() => parseChecklist(body), [body]);

  // Nothing parsed as a checklist — render the raw children as markdown.
  if (total === 0) {
    return (
      <BlockShell label="Checklist" accent="text-emerald-400">
        <BlockMarkdown>{body}</BlockMarkdown>
      </BlockShell>
    );
  }

  return (
    <BlockShell
      label="Checklist"
      accent="text-emerald-400"
      meta={
        <span className="font-mono text-xs text-muted-foreground">
          {done}/{total} done
        </span>
      }
    >
      <ul className="grid gap-2">
        {items.map((item, index) => (
          <li key={index} className="flex items-start gap-2.5">
            <HugeiconsIcon
              icon={item.checked ? CheckmarkSquare02Icon : SquareIcon}
              size={18}
              className={cn(
                "mt-0.5 shrink-0",
                item.checked ? "text-emerald-400" : "text-muted-foreground/60",
              )}
              aria-hidden
            />
            <span className="min-w-0 flex-1">
              <span
                className={cn(
                  "block break-words text-sm",
                  item.checked
                    ? "text-muted-foreground line-through"
                    : "text-foreground",
                )}
              >
                {item.label}
              </span>
              {item.note && (
                <span className="block break-words text-xs text-muted-foreground">
                  {item.note}
                </span>
              )}
            </span>
          </li>
        ))}
      </ul>
    </BlockShell>
  );
}
