import { HugeiconsIcon } from "@hugeicons/react";
import {
  Alert02Icon,
  AlertCircleIcon,
  CheckmarkCircle02Icon,
  CancelCircleIcon,
  Shield01Icon,
} from "@hugeicons/core-free-icons";
import { Badge } from "@/components/ui/badge";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";
import type {
  ProposalGateStatus,
  ProposalRefinementStatus,
} from "@/api/types";

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
  gateStatus,
  refinement,
}: {
  gateStatus: ProposalGateStatus | null;
  refinement: ProposalRefinementStatus | null;
}) {
  // If there's no gate status and no refinement, don't render anything.
  if (!gateStatus && !refinement) return null;

  const isReady = gateStatus?.ready ?? true;
  const hasBlockedReasons =
    gateStatus && gateStatus.blocked_explanations.length > 0;

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
          {gateStatus.judge_verdict_body && (
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

      {/* Active-refinement note */}
      {refinement?.active && (
        <p className="text-xs text-muted-foreground">
          Autonomous tribunal in progress: Adversary, Advocate, and Judge refine
          the spec automatically. You will be asked to accept or reject the
          converged result.
        </p>
      )}
    </div>
  );
}
