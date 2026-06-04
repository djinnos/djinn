import { CheckmarkCircle02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "@/lib/utils";
import { PROPOSAL_STATUS_META, type ProposalStatus } from "./proposalStatus";

/** Linear-style colored status circle for a proposal. */
export function StatusIcon({ status, size = 14 }: { status: string; size?: number }) {
  const meta = PROPOSAL_STATUS_META[status as ProposalStatus];
  if (meta?.done) {
    return (
      <HugeiconsIcon icon={CheckmarkCircle02Icon} size={size} className="shrink-0 text-green-500" />
    );
  }
  return (
    <span
      title={meta?.label ?? status}
      style={{ width: size, height: size }}
      className={cn("inline-block shrink-0 rounded-full", meta?.dot ?? "bg-muted-foreground/40")}
    />
  );
}
