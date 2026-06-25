import { useCallback, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  Alert02Icon,
  AlertCircleIcon,
  CheckmarkCircle02Icon,
  CancelCircleIcon,
  Shield01Icon,
} from "@hugeicons/core-free-icons";
import { callMcpTool } from "@/api/mcpClient";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";
import { showToast } from "@/lib/toast";
import { DiffView } from "@/components/proposals/DiffView";
import type {
  ProposalGateStatus,
  ProposalRefinementStatus,
  CheckpointRevision,
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
 * unresolved blocking debate rows, needs-evidence parked spike state, checkpoint
 * revision diff approval/edit/reject, and disabled/blocked gate explanations
 * naming exact failures.
 *
 * The component renders backend-provided status only — it does NOT recompute
 * readiness logic client-side.
 */
export function ReadinessPanel({
  gateStatus,
  refinement,
  proposalId,
  pendingRevisions,
  onChanged,
}: {
  gateStatus: ProposalGateStatus | null;
  refinement: ProposalRefinementStatus | null;
  proposalId: string;
  pendingRevisions: CheckpointRevision[];
  onChanged: () => void;
}) {
  const [actionBusy, setActionBusy] = useState<number | null>(null);
  const [diffOpen, setDiffOpen] = useState<number | null>(null);

  const handleApprove = useCallback(
    async (seq: number) => {
      setActionBusy(seq);
      try {
        const res = await callMcpTool(
          "proposal_refinement_checkpoint_approve" as any,
          { proposal_id: proposalId, revision_seq: seq },
        );
        if (res.error) throw new Error(res.error);
        showToast.success("Checkpoint revision approved");
        onChanged();
      } catch (e) {
        showToast.error("Failed to approve revision", {
          description: (e as Error).message,
        });
      } finally {
        setActionBusy(null);
      }
    },
    [proposalId, onChanged],
  );

  const handleReject = useCallback(
    async (seq: number) => {
      setActionBusy(seq);
      try {
        const res = await callMcpTool(
          "proposal_refinement_checkpoint_reject" as any,
          { proposal_id: proposalId, revision_seq: seq },
        );
        if (res.error) throw new Error(res.error);
        showToast.success("Checkpoint revision rejected");
        onChanged();
      } catch (e) {
        showToast.error("Failed to reject revision", {
          description: (e as Error).message,
        });
      } finally {
        setActionBusy(null);
      }
    },
    [proposalId, onChanged],
  );

  // If there's no gate status and no refinement, don't render anything.
  if (!gateStatus && !refinement) return null;

  const isReady = gateStatus?.ready ?? true;
  const isCheckpoint =
    refinement?.update_authority === "checkpoint";
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
          {gateStatus.pending_checkpoint && (
            <Badge variant="outline" className="text-xs">
              Pending checkpoint
            </Badge>
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

      {/* Checkpoint mode: pending revisions with diff inspection */}
      {isCheckpoint && pendingRevisions.length > 0 && (
        <div className="space-y-2 border-t pt-2">
          <Label className="text-xs uppercase text-muted-foreground">
            Pending revisions ({pendingRevisions.length})
          </Label>
          {pendingRevisions.map((rev) => (
            <div
              key={rev.seq}
              className="space-y-1 rounded-md border border-amber-200 bg-amber-50 p-2 dark:border-amber-800 dark:bg-amber-950/30"
            >
              <div className="flex items-center justify-between gap-2">
                <div className="flex items-center gap-2">
                  <Badge variant="outline" className="text-xs">
                    Round {rev.round ?? "?"}
                  </Badge>
                  {rev.author_model && (
                    <span className="text-xs text-muted-foreground">
                      {rev.author_model}
                    </span>
                  )}
                  <span className="text-xs text-muted-foreground">
                    #{rev.seq}
                  </span>
                </div>
                <div className="flex items-center gap-1">
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-6 text-xs"
                    onClick={() =>
                      setDiffOpen(diffOpen === rev.seq ? null : rev.seq)
                    }
                  >
                    {diffOpen === rev.seq ? "Hide diff" : "Diff"}
                  </Button>
                  <Button
                    size="sm"
                    variant="default"
                    className="h-6 text-xs"
                    disabled={actionBusy === rev.seq}
                    onClick={() => handleApprove(rev.seq)}
                  >
                    {actionBusy === rev.seq ? "…" : "Approve"}
                  </Button>
                  <Button
                    size="sm"
                    variant="outline"
                    className="h-6 text-xs"
                    disabled={actionBusy === rev.seq}
                    onClick={() => handleReject(rev.seq)}
                  >
                    {actionBusy === rev.seq ? "…" : "Reject"}
                  </Button>
                </div>
              </div>
              {rev.title && (
                <p className="text-xs font-medium">{rev.title}</p>
              )}
              {rev.body_preview && (
                <p className="line-clamp-2 text-xs text-muted-foreground">
                  {rev.body_preview}
                </p>
              )}
              {diffOpen === rev.seq && rev.body_preview && (
                <div className="mt-1">
                  <DiffView
                    before=""
                    after={rev.body_preview}
                  />
                  <p className="mt-1 text-[10px] text-muted-foreground">
                    Preview of proposed revision — full diff available after
                    approval.
                  </p>
                </div>
              )}
            </div>
          ))}
        </div>
      )}

      {/* Mode explanation */}
      {refinement?.active && (
        <p className="text-xs text-muted-foreground">
          Checkpoint mode: advocate revisions require explicit approval before
          they are applied.
        </p>
      )}
    </div>
  );
}
