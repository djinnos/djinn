import {
  Archive02Icon,
  CancelCircleIcon,
  CheckmarkCircle02Icon,
  CircleArrowUpIcon,
  DashedLineCircleIcon,
  SearchCircleIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "@/lib/utils";
import { PROPOSAL_STATUS_META, type ProposalStatus } from "./proposalStatus";

type IconObj = typeof CheckmarkCircle02Icon;

// Per-status Hugeicons glyph + color. `approved`/`building` fall back to the
// colored dot from PROPOSAL_STATUS_META.
//
// Note: a few requested names aren't in the installed @hugeicons free set, so
// the closest matches are used: DashedLineCircle (draft), CircleArrowUp
// (superseded), Archive02 (archived).
const STATUS_ICONS: Partial<Record<ProposalStatus, { icon: IconObj; className: string }>> = {
  draft: { icon: DashedLineCircleIcon, className: "text-muted-foreground" },
  in_review: { icon: SearchCircleIcon, className: "text-amber-500" },
  done: { icon: CheckmarkCircle02Icon, className: "text-green-500" },
  rejected: { icon: CancelCircleIcon, className: "text-red-500" },
  superseded: { icon: CircleArrowUpIcon, className: "text-muted-foreground" },
  archived: { icon: Archive02Icon, className: "text-muted-foreground" },
};

/** Linear-style status indicator for a proposal. */
export function StatusIcon({ status, size = 14 }: { status: string; size?: number }) {
  const mapped = STATUS_ICONS[status as ProposalStatus];
  if (mapped) {
    return (
      <HugeiconsIcon icon={mapped.icon} size={size} className={cn("shrink-0", mapped.className)} />
    );
  }
  // approved / building (and any unknown) → colored dot.
  const meta = PROPOSAL_STATUS_META[status as ProposalStatus];
  return (
    <span
      title={meta?.label ?? status}
      style={{ width: size, height: size }}
      className={cn("inline-block shrink-0 rounded-full", meta?.dot ?? "bg-muted-foreground/40")}
    />
  );
}
