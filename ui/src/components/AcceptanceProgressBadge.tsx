import type { AcceptanceCriterion } from "@/api/types";
import {
  Progress01Icon,
  Progress02Icon,
  Progress03Icon,
  Progress04Icon,
  Tick01Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "@/lib/utils";

function acProgressIcon(met: number, total: number) {
  if (met === total) return Tick01Icon;
  if (met === 0) return Progress01Icon;
  const pct = met / total;
  if (pct <= 0.25) return Progress02Icon;
  if (pct <= 0.5) return Progress03Icon;
  return Progress04Icon;
}

/**
 * Compact "met/total" acceptance-criteria progress pill, shared by the task
 * card and the proposals list. Renders nothing when there are no criteria.
 * Accepts the raw `acceptance_criteria` array (items may be `{criterion, met}`
 * objects or bare strings — strings count toward the total but never as met).
 */
type AcceptanceProgressBadgeProps =
  | { criteria: ReadonlyArray<AcceptanceCriterion | string> | null | undefined; met?: never; total?: never; className?: string }
  | { criteria?: never; met: number; total: number; className?: string };

export function AcceptanceProgressBadge(props: AcceptanceProgressBadgeProps) {
  const { className } = props;
  const items = "criteria" in props ? props.criteria ?? [] : [];
  const total = "total" in props ? props.total : items.length;
  if (total === 0) return null;
  const met = props.met !== undefined
    ? props.met
    : items.filter((c) => typeof c === "object" && c !== null && c.met).length;

  return (
    <span
      className={cn(
        "inline-flex items-center gap-0.5 rounded px-1 py-px text-[10px] font-medium",
        met === total
          ? "bg-emerald-500/15 text-emerald-400"
          : met === 0
            ? "bg-zinc-500/10 text-muted-foreground"
            : "bg-amber-500/15 text-amber-400",
        className,
      )}
    >
      <HugeiconsIcon icon={acProgressIcon(met, total)} size={10} className="shrink-0" />
      {met}/{total}
    </span>
  );
}
