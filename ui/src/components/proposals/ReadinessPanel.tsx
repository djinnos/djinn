import { useCallback, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  Alert02Icon,
  AlertCircleIcon,
  CheckmarkCircle02Icon,
  CancelCircleIcon,
  Shield01Icon,
} from "@hugeicons/core-free-icons";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { callMcpTool } from "@/api/mcpClient";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import type {
  ProposalGateStatus,
  ProposalRefinementStatus,
} from "@/api/types";
import { BlockMarkdown } from "./blocks/BlockShell";

/**
 * Human-readable label for a DoR check name.
 */
function checkLabel(check: string): string {
  const map: Record<string, string> = {
    problem_coverage: "Problem coverage",
    scope_coverage: "Scope coverage",
    objective_coverage: "Objective / outcomes",
    target_count: "Target projects",
    acceptance_criteria_count: "Acceptance criteria",
    vague_acceptance_criteria: "Vague acceptance criterion",
    grounding: "Grounding",
    dependencies_coverage: "Dependencies / coordination",
    open_questions_risks_coverage: "Open questions / risks",
  };
  return map[check] ?? check;
}

/**
 * Readiness panel for a proposal.
 *
 * Displays deterministic DoR status, latest Judge verdict, adversary dry count,
 * unresolved blocking debate rows, needs-evidence parked spike state, and
 * disabled/blocked gate explanations naming exact failures.
 *
 * The component renders backend-provided status only — it does NOT recompute
 * readiness logic client-side. The autonomous tribunal's converged result and
 * the single human accept/reject review live in `ProposalRefinement`.
 */
export function ReadinessPanel({
  proposalId,
  gateStatus,
  refinement,
  onChanged,
}: {
  proposalId?: string;
  gateStatus: ProposalGateStatus | null;
  refinement: ProposalRefinementStatus | null;
  onChanged?: () => void;
}) {
  const [overrideReason, setOverrideReason] = useState("");
  const [overrideBusy, setOverrideBusy] = useState(false);

  const isReady = gateStatus?.ready ?? true;
  const hasBlockedReasons =
    gateStatus && gateStatus.blocked_explanations.length > 0;
  const judgeVerdictBody = gateStatus?.judge_verdict_body?.trim();
  const showOverrideControls = Boolean(
    proposalId && gateStatus && !gateStatus.ready && !gateStatus.human_override_active,
  );

  const handleOverride = useCallback(async () => {
    const reason = overrideReason.trim();
    if (!proposalId || !gateStatus || !reason) return;

    setOverrideBusy(true);
    try {
      const res = await callMcpTool("proposal_verdict_override", {
        proposal_id: proposalId,
        overridden_verdict_entry_id: gateStatus.judge_verdict_id ?? undefined,
        reason,
      });
      if (res.error) throw new Error(res.error);
      showToast.success("Human override recorded");
      setOverrideReason("");
      onChanged?.();
    } catch (e) {
      showToast.error("Failed to record human override", {
        description: (e as Error).message,
      });
    } finally {
      setOverrideBusy(false);
    }
  }, [gateStatus, proposalId, overrideReason, onChanged]);

  // If there's no gate status and no refinement, don't render anything.
  if (!gateStatus && !refinement) return null;

  return (
    <div className="space-y-3 rounded-md border p-3">
      {/* Header */}
      <div className="flex items-center justify-between">
        <Label className="text-xs uppercase text-muted-foreground">
          Readiness
        </Label>
        <div className="flex items-center gap-2">
          {isReady ? (
            <Badge variant="default" className="gap-1 text-xs">
              <HugeiconsIcon icon={CheckmarkCircle02Icon} size={12} />
              Ready
            </Badge>
          ) : (
            <Badge variant="destructive" className="gap-1 text-xs">
              <HugeiconsIcon icon={CancelCircleIcon} size={12} />
              Blocked
            </Badge>
          )}
          {gateStatus?.human_override_active && (
            <Badge variant="outline" className="gap-1 text-xs">
              <HugeiconsIcon icon={Shield01Icon} size={12} />
              Human override
            </Badge>
          )}
        </div>
      </div>

      {/* DoR status */}
      {gateStatus && (
        <div className="space-y-1">
          <div className="flex items-center gap-2 text-sm">
            <HugeiconsIcon
              icon={
                gateStatus.dor_ready
                  ? CheckmarkCircle02Icon
                  : CancelCircleIcon
              }
              size={14}
              className={
                gateStatus.dor_ready ? "text-green-600" : "text-destructive"
              }
            />
            <span
              className={cn(
                "font-medium",
                gateStatus.dor_ready
                  ? "text-green-700 dark:text-green-400"
                  : "text-destructive",
              )}
            >
              Definition of Ready: {gateStatus.dor_ready ? "Pass" : "Fail"}
            </span>
          </div>
          {gateStatus.dor_failures.length > 0 && (
            <ul className="ml-5 space-y-0.5 text-xs text-muted-foreground">
              {gateStatus.dor_failures.map((f, i) => (
                <li key={i}>
                  <span className="font-medium">{checkLabel(f.check)}</span>:{" "}
                  {f.message}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      {/* Tribunal metrics */}
      {gateStatus && (
        <div className="flex flex-wrap gap-3 text-sm">
          {judgeVerdictBody && (
            <div className="space-y-0.5">
              <span className="text-muted-foreground">Judge verdict</span>
              <Badge
                variant={
                  gateStatus.judge_needs_work ? "destructive" : "default"
                }
                className="ml-1 text-xs"
              >
                {gateStatus.judge_needs_work ? "Needs-work" : "Ready"}
              </Badge>
            </div>
          )}
          {gateStatus.adversary_dry_count > 0 && (
            <span className="text-muted-foreground">
              Adversary dry:{" "}
              <span className="font-mono font-medium text-foreground">
                {gateStatus.adversary_dry_count}
              </span>
            </span>
          )}
          {gateStatus.unresolved_blocking_count > 0 && (
            <span className="text-muted-foreground">
              Blocking rows:{" "}
              <span className="font-mono font-medium text-destructive">
                {gateStatus.unresolved_blocking_count}
              </span>
            </span>
          )}
        </div>
      )}

      {judgeVerdictBody && (
        <div className="space-y-2 rounded-md border bg-muted/30 p-3">
          <div className="flex items-center justify-between gap-2">
            <Label className="text-xs uppercase text-muted-foreground">
              Judge reasoning
            </Label>
            <Badge
              variant={gateStatus?.judge_needs_work ? "destructive" : "default"}
              className="text-xs"
            >
              {gateStatus?.judge_needs_work ? "Needs-work" : "Ready"}
            </Badge>
          </div>
          <BlockMarkdown>{judgeVerdictBody}</BlockMarkdown>
        </div>
      )}

      {gateStatus?.human_override_active && (
        <div className="flex items-start gap-2 rounded-md border border-primary/30 bg-primary/5 p-2">
          <HugeiconsIcon
            icon={Shield01Icon}
            size={14}
            className="mt-0.5 shrink-0 text-primary"
          />
          <p className="text-xs text-muted-foreground">
            A human override is active for this proposal revision, so the current
            DoR or Judge block has already been audited and does not need another
            override.
          </p>
        </div>
      )}

      {showOverrideControls && (
        <div className="space-y-2 rounded-md border border-amber-200 bg-amber-50 p-3 dark:border-amber-800 dark:bg-amber-950/30">
          <div className="space-y-1">
            <Label
              htmlFor="readiness-override-reason"
              className="text-xs uppercase text-muted-foreground"
            >
              Record human override
            </Label>
            <p className="text-xs text-muted-foreground">
              Use this audited action only for DoR or tribunal false positives.
              The backend records the reason and decides whether the override is
              allowed for this proposal revision.
            </p>
          </div>
          <Textarea
            id="readiness-override-reason"
            value={overrideReason}
            onChange={(e) => setOverrideReason(e.target.value)}
            placeholder="Why is it safe to override this readiness block?"
            aria-label="Human override audit reason"
            disabled={overrideBusy}
            className="min-h-[72px] bg-background"
          />
          <Button
            size="sm"
            variant="destructive"
            disabled={overrideBusy || !overrideReason.trim()}
            onClick={handleOverride}
          >
            {overrideBusy ? "Recording…" : "Override DoR and proceed"}
          </Button>
        </div>
      )}

      {/* Needs-evidence spike parking */}
      {gateStatus?.needs_evidence && (
        <div className="flex items-start gap-2 rounded-md border border-amber-200 bg-amber-50 p-2 dark:border-amber-800 dark:bg-amber-950/30">
          <HugeiconsIcon
            icon={AlertCircleIcon}
            size={14}
            className="mt-0.5 shrink-0 text-amber-600"
          />
          <div className="space-y-0.5 text-xs">
            <p className="font-medium text-amber-800 dark:text-amber-200">
              Parked: needs-evidence spike
            </p>
            <p className="text-muted-foreground">
              Claim: {gateStatus.needs_evidence.claim}
            </p>
            <p className="text-muted-foreground">
              Spike task: {gateStatus.needs_evidence.spike_short_id} —{" "}
              {gateStatus.needs_evidence.spike_status}
            </p>
          </div>
        </div>
      )}

      {/* Blocked explanations */}
      {hasBlockedReasons && (
        <div className="space-y-1">
          <Label className="text-xs uppercase text-muted-foreground">
            Blocked because
          </Label>
          <ul className="space-y-1">
            {gateStatus!.blocked_explanations.map((explanation, i) => (
              <li
                key={i}
                className="flex items-start gap-1.5 text-xs text-destructive"
              >
                <HugeiconsIcon
                  icon={Alert02Icon}
                  size={12}
                  className="mt-0.5 shrink-0"
                />
                <span>{explanation}</span>
              </li>
            ))}
          </ul>
        </div>
      )}

      {/* Active-refinement note — only while the tribunal is still running.
          Once it converges (awaiting_review) the refinement panel above shows
          the verdict + accept/reject, so this "in progress" line would be
          stale/contradictory. */}
      {refinement?.active && !refinement?.awaiting_review && (
        <p className="text-xs text-muted-foreground">
          Autonomous tribunal in progress: Adversary, Advocate, and Judge refine
          the spec automatically. You will be asked to accept or reject the
          converged result.
        </p>
      )}
    </div>
  );
}
