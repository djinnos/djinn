/**
 * ImpactFlowModal — PR D4 blast-radius visualization.
 *
 * Opens from the "Show impact" button on the Code Graph right-rail (D3 wires
 * the trigger; D4 ships the modal). Renders a `code_graph impact` response
 * as a Mermaid `flowchart TD`, bucketing entries into depth-based subgraphs
 * so the visual reads as concentric rings around the queried symbol.
 *
 * Surfaces the PR C3 risk + summary fields prominently:
 *   - Risk badge (LOW/MEDIUM/HIGH/CRITICAL) with a color hue chosen to match
 *     the upstream gating: HIGH/CRITICAL get destructive coloring so the eye
 *     lands on the worst-case ripples first.
 *   - 1-line human summary verbatim so chat citations and modal stay in sync.
 *
 * Scope rules:
 *   - No chat-citation logic (deferred to D5).
 *   - Doesn't fetch — receives the parsed `ImpactDetailedResult` from the
 *     parent (D3 owns the round-trip).
 */

import { useMemo } from "react";

import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { MermaidDiagram } from "@/components/MermaidDiagram";
import {
  buildImpactMermaid,
  type ImpactDetailedResult,
  type ImpactRisk,
} from "@/components/codegraph/impactMermaid";
import { cn } from "@/lib/utils";

export type {
  ImpactDetailedResult,
  ImpactEntry,
  ImpactRisk,
} from "@/components/codegraph/impactMermaid";

interface ImpactFlowModalProps {
  open: boolean;
  onClose: () => void;
  impact: ImpactDetailedResult;
}

const RISK_BADGE: Record<
  ImpactRisk,
  { label: string; className: string; description: string }
> = {
  LOW: {
    label: "LOW",
    className: "bg-emerald-500/15 text-emerald-700 dark:text-emerald-300",
    description: "Limited blast radius",
  },
  MEDIUM: {
    label: "MEDIUM",
    className: "bg-amber-500/15 text-amber-700 dark:text-amber-300",
    description: "Cross-module callers — review touched files",
  },
  HIGH: {
    label: "HIGH",
    className: "bg-orange-500/15 text-orange-700 dark:text-orange-300",
    description:
      "Many direct callers or modules — pre-clean dead/deprecated callers",
  },
  CRITICAL: {
    label: "CRITICAL",
    className: "bg-destructive/15 text-destructive",
    description: "Repo-wide ripple — schedule explicit migration work",
  },
};

export function ImpactFlowModal({ open, onClose, impact }: ImpactFlowModalProps) {
  const mermaidSource = useMemo(
    () => buildImpactMermaid(impact),
    [impact],
  );

  const risk = impact.risk ?? null;
  const riskMeta = risk ? RISK_BADGE[risk] : null;

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) onClose();
      }}
    >
      <DialogContent
        className="sm:max-w-3xl"
        data-testid="impact-flow-modal"
      >
        <DialogHeader>
          <div className="flex items-center justify-between gap-3">
            <DialogTitle className="text-base">Blast radius</DialogTitle>
            {riskMeta && (
              <Badge
                className={cn(
                  "rounded-md px-2 py-0.5 text-[11px] font-semibold tracking-wide uppercase",
                  riskMeta.className,
                )}
                data-testid="impact-risk-badge"
                data-risk={risk ?? undefined}
              >
                {riskMeta.label}
              </Badge>
            )}
          </div>
          <DialogDescription className="text-xs text-muted-foreground">
            {impact.target_label ?? impact.key}
          </DialogDescription>
        </DialogHeader>

        {impact.summary && (
          <p
            className="text-sm leading-snug text-foreground"
            data-testid="impact-summary"
          >
            {impact.summary}
          </p>
        )}
        {riskMeta?.description && (
          <p className="text-xs text-muted-foreground">
            {riskMeta.description}
          </p>
        )}

        <div className="max-h-[60vh] overflow-auto rounded-md border border-border/40 bg-background/50 p-4">
          {impact.entries.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              No transitive dependents — this symbol stands alone in the
              current snapshot.
            </p>
          ) : (
            <MermaidDiagram source={mermaidSource} />
          )}
        </div>

        <DialogFooter>
          <DialogClose render={<Button variant="outline" onClick={onClose} />}>
            Close
          </DialogClose>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
