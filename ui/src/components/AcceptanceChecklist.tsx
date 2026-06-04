import { CheckmarkCircle02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { AcceptanceCriterion } from "@/api/types";

export type AcceptanceCriterionInput = string | AcceptanceCriterion;

function parseCriterion(
  raw: AcceptanceCriterionInput,
): { criterion: string; met: boolean } {
  if (typeof raw === "string") return { criterion: raw, met: false };
  return { criterion: raw.criterion, met: Boolean(raw.met) };
}

/**
 * Shared acceptance-criteria checklist used by both tasks and proposals.
 * On a task, `met` means "implemented"; on a proposal it means "agreed during
 * scoping". Pass `onToggle` to make the checks interactive.
 */
export function AcceptanceChecklist({
  criteria,
  onToggle,
  emptyLabel = "No acceptance criteria",
}: {
  criteria: AcceptanceCriterionInput[];
  onToggle?: (index: number, met: boolean) => void;
  emptyLabel?: string;
}) {
  const items = criteria.length
    ? criteria.map(parseCriterion)
    : [{ criterion: emptyLabel, met: false }];
  const interactive = !!onToggle && criteria.length > 0;

  return (
    <ul className="space-y-2">
      {items.map((item, idx) => {
        const indicator = item.met ? (
          <HugeiconsIcon
            icon={CheckmarkCircle02Icon}
            size={16}
            className="text-green-500"
          />
        ) : (
          <span className="inline-block h-4 w-4 rounded-full border-2 border-muted-foreground/40" />
        );
        return (
          <li key={`${item.criterion}-${idx}`} className="flex items-start gap-2 text-sm">
            {interactive ? (
              <button
                type="button"
                aria-pressed={item.met}
                onClick={() => onToggle?.(idx, !item.met)}
                className="mt-0.5 shrink-0 transition-opacity hover:opacity-70"
              >
                {indicator}
              </button>
            ) : (
              <span className="mt-0.5 shrink-0">{indicator}</span>
            )}
            <span>{item.criterion}</span>
          </li>
        );
      })}
    </ul>
  );
}
