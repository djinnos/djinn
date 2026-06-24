import { useState, useCallback } from "react";
import { callMcpTool } from "@/api/mcpClient";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { showToast } from "@/lib/toast";
import type { ProposalRefinementStatus } from "@/api/types";

/**
 * Human-readable label for the stop reason.
 */
function stopReasonLabel(reason: string): string {
  switch (reason) {
    case "adversary_dry":
      return "Adversary exhausted (no new blocking objections)";
    case "round_cap":
      return "Round cap reached";
    case "spawn_cap":
      return "Agent spawn cap reached";
    case "repeated_objection":
      return "Repeated objection detected";
    case "agent_failure":
      return "Agent failure";
    default:
      return reason;
  }
}

/**
 * Badge variant for the update authority mode.
 */
function authorityBadgeVariant(
  authority: string,
): "default" | "secondary" | "outline" {
  return authority === "auto_accept" ? "default" : "secondary";
}

/**
 * Proposal refinement kickoff and status component.
 *
 * Shows a "Start refinement" button when refinement hasn't been started,
 * and a status panel when refinement is active or has stopped.
 *
 * Copy explains:
 * - Checkpoint mode: advocate revisions require explicit approval.
 * - Auto-accept mode: revisions are applied automatically.
 * - Same-model fallback: when diverse models are unavailable, the tribunal
 *   falls back to the same model — this is not an error.
 */
export function ProposalRefinement({
  proposalId,
  status,
  canStart,
  onChanged,
}: {
  proposalId: string;
  status: ProposalRefinementStatus | null;
  canStart: boolean;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [authority, setAuthority] = useState<string>("checkpoint");

  const handleStart = useCallback(async () => {
    setBusy(true);
    try {
      const res = await callMcpTool("proposal_refinement_start", {
        proposal_id: proposalId,
        update_authority: authority,
      });
      if (res.error) throw new Error(res.error);
      showToast.success("Refinement started");
      onChanged();
    } catch (e) {
      showToast.error("Failed to start refinement", {
        description: (e as Error).message,
      });
    } finally {
      setBusy(false);
    }
  }, [proposalId, authority, onChanged]);

  // Active or stopped refinement status panel.
  if (status && (status.active || status.stop_reason)) {
    return (
      <div className="space-y-2 rounded-md border p-3">
        <div className="flex items-center justify-between">
          <Label className="text-xs uppercase text-muted-foreground">
            Refinement
          </Label>
          <div className="flex items-center gap-2">
            {status.active ? (
              <Badge variant="default" className="text-xs">
                Active
              </Badge>
            ) : (
              <Badge variant="secondary" className="text-xs">
                Stopped
              </Badge>
            )}
            <Badge
              variant={authorityBadgeVariant(status.update_authority)}
              className="text-xs"
            >
              {status.update_authority === "auto_accept"
                ? "Auto-accept"
                : "Checkpoint"}
            </Badge>
          </div>
        </div>

        <div className="flex flex-wrap gap-3 text-sm">
          {status.current_round != null && (
            <span className="text-muted-foreground">
              Round{" "}
              <span className="font-mono font-medium text-foreground">
                {status.current_round}
              </span>
            </span>
          )}
          <span className="text-muted-foreground">
            Entries{" "}
            <span className="font-mono font-medium text-foreground">
              {status.total_entries}
            </span>
          </span>
          {status.dry_rounds > 0 && (
            <span className="text-muted-foreground">
              Dry rounds{" "}
              <span className="font-mono font-medium text-foreground">
                {status.dry_rounds}
              </span>
            </span>
          )}
        </div>

        {status.stop_reason && (
          <p className="text-xs text-muted-foreground">
            Stopped: {stopReasonLabel(status.stop_reason)}
          </p>
        )}

        {status.update_authority === "checkpoint" && status.active && (
          <p className="text-xs text-muted-foreground">
            Advocate revisions require explicit approval before they are applied.
            In auto-accept mode, revisions are applied automatically.
          </p>
        )}

        <p className="text-xs text-muted-foreground">
          The tribunal uses best-effort cross-model diversity. When alternate
          models are unavailable, it falls back to the same model — this is
          expected behavior, not an error.
        </p>
      </div>
    );
  }

  // Kickoff affordance — only shown when refinement hasn't started.
  if (!canStart) return null;

  return (
    <div className="space-y-3 rounded-md border border-primary/40 bg-primary/5 p-3">
      <Label className="text-xs uppercase text-muted-foreground">
        Proposal refinement
      </Label>
      <p className="text-sm">
        Run the Advocate/Adversary/Judge tribunal to refine this proposal before
        graduation. The bounded loop stops after consecutive dry adversary
        rounds.
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-sm">Mode</span>
        <Select value={authority} onValueChange={setAuthority}>
          <SelectTrigger className="h-8 w-[200px] text-sm">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="checkpoint">Checkpoint</SelectItem>
            <SelectItem value="auto_accept">Auto-accept</SelectItem>
          </SelectContent>
        </Select>
        <Button size="sm" disabled={busy} onClick={handleStart}>
          {busy ? "Starting…" : "Start refinement"}
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">
        {authority === "checkpoint"
          ? "Checkpoint mode: advocate revisions are proposed for approval before they are applied."
          : "Auto-accept mode: advocate revisions are applied automatically as proposal updates."}
        {" "}
        Same-model fallback is used when diverse models are unavailable.
      </p>
    </div>
  );
}
