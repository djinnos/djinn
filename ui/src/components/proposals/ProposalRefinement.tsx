import { useState, useCallback, useEffect } from "react";
import { useQuery } from "@tanstack/react-query";
import { callMcpTool } from "@/api/mcpClient";
import { usersQueryOptions } from "@/api/queryOptions";
import { userDisplayName, type OrgUser } from "@/api/users";
import { useAuthUser } from "@/components/AuthGate";
import { UserAvatar } from "@/components/UserAvatar";
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
import { BlockMarkdown } from "./blocks/BlockShell";

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

function judgeSummaryText(summary: string | null | undefined): string {
  return summary && summary.trim() ? summary : "The tribunal converged.";
}

/**
 * Proposal refinement kickoff and status component.
 *
 * Shows a "Start refinement" button when refinement hasn't been started.
 *
 * Refinement runs as an autonomous tribunal (Adversary → Advocate → Judge,
 * looping). The human is no longer a per-revision approver: when the tribunal
 * converges it parks for a single human accept/reject of the full refined
 * result (`awaiting_review`).
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
  const me = useAuthUser();
  const usersQuery = useQuery(usersQueryOptions());
  const users: OrgUser[] = usersQuery.data ?? [];
  const userFor = (id: string | null | undefined) =>
    id ? users.find((u) => u.id === id) : undefined;
  const nameFor = (id: string | null | undefined) => {
    if (!id) return "unknown";
    const u = userFor(id);
    return u ? userDisplayName(u) : id;
  };
  // User the tribunal runs are attributed to (task owner + model scope),
  // mirroring the kick-off owner picker. Defaults to the current user; the
  // backend falls back to the proposal author when left blank.
  const [owner, setOwner] = useState<string>("");
  useEffect(() => {
    if (!owner && me?.id) setOwner(me.id);
  }, [owner, me?.id]);

  // Optional feedback the human attaches when accepting/rejecting the result.
  // Required to demand another tribunal round (sent as the request reason).
  const [feedback, setFeedback] = useState<string>("");
  const feedbackBlank = !feedback.trim();

  const handleResolve = useCallback(
    async (decision: "accept" | "reject") => {
      setBusy(true);
      try {
        const res = await callMcpTool("proposal_refinement_resolve" as any, {
          proposal_id: proposalId,
          decision,
          feedback: feedback || undefined,
        });
        if (res.error) throw new Error(res.error);
        showToast.success(
          decision === "accept"
            ? "Refined spec accepted"
            : "Refinement rejected — reverted to your original",
        );
        onChanged();
      } catch (e) {
        showToast.error(
          decision === "accept"
            ? "Failed to accept refined spec"
            : "Failed to reject refinement",
          { description: (e as Error).message },
        );
      } finally {
        setBusy(false);
      }
    },
    [proposalId, feedback, onChanged],
  );

  // Send the textarea feedback into another tribunal round from the
  // awaiting_review state. The feedback is passed as the request `reason` so
  // the next round receives the human's notes. Unlike accept/reject, a reason
  // is required — the action is disabled until non-blank feedback is entered.
  const handleDemandRound = useCallback(async () => {
    setBusy(true);
    try {
      const res = await callMcpTool("proposal_refinement_demand_round", {
        proposal_id: proposalId,
        reason: feedback.trim() || undefined,
      });
      if (res.error || !res.accepted) {
        throw new Error(res.error ?? "Refinement round was not accepted");
      }
      showToast.success("Feedback sent — another tribunal round started");
      onChanged();
    } catch (e) {
      showToast.error("Failed to send feedback for another round", {
        description: (e as Error).message,
      });
    } finally {
      setBusy(false);
    }
  }, [proposalId, feedback, onChanged]);

  const handleStart = useCallback(async () => {
    setBusy(true);
    try {
      const res = await callMcpTool("proposal_refinement_start", {
        proposal_id: proposalId,
        owner_user_id: owner || undefined,
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
  }, [proposalId, owner, onChanged]);

  // Active or stopped refinement status panel.
  if (status && (status.active || status.stop_reason)) {
    return (
      <div className="space-y-2 rounded-md border p-3">
        <div className="flex items-center justify-between">
          <Label className="text-xs uppercase text-muted-foreground">
            Refinement
          </Label>
          <div className="flex items-center gap-2">
            {status.active && status.awaiting_review ? (
              <Badge variant="secondary" className="text-xs">
                Awaiting review
              </Badge>
            ) : status.active ? (
              <Badge variant="default" className="text-xs">
                Active
              </Badge>
            ) : (
              <Badge variant="secondary" className="text-xs">
                Stopped
              </Badge>
            )}
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

        {/* Stopped — let the user run a fresh tribunal from here. Without this
            a stopped/interrupted refinement is a dead end (e.g. a run lost
            across a deploy leaves nothing actionable in the UI). */}
        {!status.active && (
          <div className="space-y-1 border-t pt-2">
            <Button size="sm" disabled={busy} onClick={handleStart}>
              {busy ? "Starting…" : "Restart refinement"}
            </Button>
            <p className="text-xs text-muted-foreground">
              Runs a fresh Adversary → Advocate → Judge tribunal from the current
              spec.
            </p>
          </div>
        )}

        {/* Converged — single human review of the full refined result. */}
        {status.active && status.awaiting_review && (
          <div className="space-y-3 rounded-md border border-primary/40 bg-primary/5 p-3">
            <Label className="text-sm font-medium">
              Tribunal converged — review the result
            </Label>
            <div className="rounded-md bg-muted/40 p-2 text-sm text-foreground">
              <BlockMarkdown>{judgeSummaryText(status.judge_summary)}</BlockMarkdown>
            </div>
            <p className="text-xs text-muted-foreground">
              The full refined spec is shown in the proposal above; the diff from
              your original is in the revision history.
            </p>
            <div className="space-y-1">
              <Label
                htmlFor="refinement-feedback"
                className="text-xs uppercase text-muted-foreground"
              >
                Feedback
              </Label>
              <textarea
                id="refinement-feedback"
                className="min-h-[72px] w-full rounded-md border bg-background p-2 text-sm"
                value={feedback}
                onChange={(e) => setFeedback(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                Optional for accept or reject. Enter feedback to send it into
                another tribunal round.
              </p>
            </div>
            <div className="flex flex-wrap items-center gap-2">
              <Button
                size="sm"
                variant="default"
                disabled={busy}
                onClick={() => handleResolve("accept")}
              >
                Accept
              </Button>
              <Button
                size="sm"
                variant="outline"
                disabled={busy || feedbackBlank}
                onClick={handleDemandRound}
                title={
                  feedbackBlank
                    ? "Enter feedback to send another tribunal round"
                    : undefined
                }
              >
                Send feedback for another round
              </Button>
              <Button
                size="sm"
                variant="outline"
                disabled={busy}
                onClick={() => handleResolve("reject")}
              >
                Reject
              </Button>
            </div>
            <p className="text-xs text-muted-foreground">
              Accept keeps the refined spec. Send feedback reruns the tribunal
              with your notes. Reject reverts to your original spec.
            </p>
          </div>
        )}

        {/* In-progress: tribunal still running autonomously. */}
        {status.active && !status.awaiting_review && (
          <div className="space-y-1 border-t pt-2">
            <Label className="text-xs uppercase text-muted-foreground">
              Refinement in progress
            </Label>
            {status.current_round != null && (
              <p className="text-xs text-muted-foreground">
                Round{" "}
                <span className="font-mono font-medium text-foreground">
                  {status.current_round}
                </span>
              </p>
            )}
            <p className="text-xs text-muted-foreground">
              Adversary → Advocate → Judge running autonomously; you'll review the
              result when it converges.
            </p>
          </div>
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
        graduation. The bounded loop runs autonomously and stops after
        consecutive dry adversary rounds.
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-sm">Attribute to</span>
        <Select
          value={owner}
          onValueChange={(v) => typeof v === "string" && setOwner(v)}
        >
          <SelectTrigger className="h-8 w-[200px] text-sm">
            {/* Render the resolved name explicitly: `owner` is set
                programmatically, so Radix never captures the selected item's
                text and SelectValue would otherwise fall back to the raw id. */}
            <SelectValue placeholder="Pick a user">
              {owner ? (
                <span className="flex items-center gap-2">
                  <UserAvatar user={userFor(owner)} className="size-4" />
                  {nameFor(owner)}
                </span>
              ) : undefined}
            </SelectValue>
          </SelectTrigger>
          <SelectContent>
            {users.map((u) => (
              <SelectItem key={u.id} value={u.id}>
                <span className="flex items-center gap-2">
                  <UserAvatar user={u} className="size-4" />
                  {userDisplayName(u)}
                </span>
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Button size="sm" disabled={busy} onClick={handleStart}>
          {busy ? "Starting…" : "Start refinement"}
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">
        The tribunal runs autonomously; when it converges you'll review the full
        refined result once. Same-model fallback is used when diverse models are
        unavailable.
      </p>
    </div>
  );
}
