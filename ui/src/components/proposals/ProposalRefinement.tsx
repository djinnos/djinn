import { useState, useCallback, useEffect } from "react";
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
import { DiffView } from "@/components/proposals/DiffView";
import type { CheckpointRevision, ProposalRefinementStatus } from "@/api/types";

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
 * Status badge for a checkpoint revision.
 */
function checkpointStatusBadge(
  rev: CheckpointRevision,
): { label: string; variant: "default" | "secondary" | "outline" } {
  return { label: `Round ${rev.round ?? "?"}`, variant: "outline" };
}

/**
 * Proposal refinement kickoff and status component.
 *
 * Shows a "Start refinement" button when refinement hasn't been started,
 * and a status panel when refinement is active or has stopped.
 *
 * In checkpoint mode, shows pending advocate revisions with approve/reject
 * actions. In auto-accept mode, revisions are applied automatically.
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
  const [pendingRevisions, setPendingRevisions] = useState<
    CheckpointRevision[]
  >([]);
  const [actionBusy, setActionBusy] = useState<number | null>(null);
  const [diffOpen, setDiffOpen] = useState<number | null>(null);

  // Fetch pending checkpoint revisions when in checkpoint mode.
  const fetchPending = useCallback(async () => {
    if (!proposalId) return;
    try {
      const res = await callMcpTool(
        "proposal_refinement_checkpoint_list" as any,
        { proposal_id: proposalId },
      );
      if (!res.error) {
        setPendingRevisions(res.pending ?? []);
      }
    } catch {
      // Silently ignore — the list endpoint may not exist on older servers.
    }
  }, [proposalId]);

  useEffect(() => {
    if (status?.update_authority === "checkpoint" && status?.active) {
      fetchPending();
    }
  }, [status?.update_authority, status?.active, status?.pending_checkpoint_count, fetchPending]);

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
        await fetchPending();
        onChanged();
      } catch (e) {
        showToast.error("Failed to approve revision", {
          description: (e as Error).message,
        });
      } finally {
        setActionBusy(null);
      }
    },
    [proposalId, fetchPending, onChanged],
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
        await fetchPending();
        onChanged();
      } catch (e) {
        showToast.error("Failed to reject revision", {
          description: (e as Error).message,
        });
      } finally {
        setActionBusy(null);
      }
    },
    [proposalId, fetchPending, onChanged],
  );

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

        {/* Pending checkpoint revisions */}
        {status.update_authority === "checkpoint" &&
          (status.pending_checkpoint_count ?? 0) > 0 && (
            <div className="space-y-2 border-t pt-2">
              <Label className="text-xs uppercase text-muted-foreground">
                Pending revisions ({status.pending_checkpoint_count})
              </Label>
              {pendingRevisions.map((rev) => {
                const badge = checkpointStatusBadge(rev);
                return (
                  <div
                    key={rev.seq}
                    className="space-y-1 rounded-md border border-amber-200 bg-amber-50 p-2"
                  >
                    <div className="flex items-center justify-between gap-2">
                      <div className="flex items-center gap-2">
                        <Badge
                          variant={badge.variant}
                          className="text-xs"
                        >
                          {badge.label}
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
                          {actionBusy === rev.seq
                            ? "…"
                            : "Approve"}
                        </Button>
                        <Button
                          size="sm"
                          variant="outline"
                          className="h-6 text-xs"
                          disabled={actionBusy === rev.seq}
                          onClick={() => handleReject(rev.seq)}
                        >
                          {actionBusy === rev.seq
                            ? "…"
                            : "Reject"}
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
                        <DiffView before="" after={rev.body_preview} />
                        <p className="mt-1 text-[10px] text-muted-foreground">
                          Preview of proposed revision — full diff available
                          after approval.
                        </p>
                      </div>
                    )}
                  </div>
                );
              })}
              {pendingRevisions.length === 0 && (
                <p className="text-xs text-muted-foreground">
                  {status.pending_checkpoint_count} pending revision(s) — refresh
                  to see details.
                </p>
              )}
            </div>
          )}

        {status.update_authority === "checkpoint" && status.active && (
          <p className="text-xs text-muted-foreground">
            {(status.pending_checkpoint_count ?? 0) > 0
              ? `${status.pending_checkpoint_count} advocate revision(s) awaiting approval.`
              : "Advocate revisions require explicit approval before they are applied."}
            {" "}
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
        <Select value={authority} onValueChange={(value) => setAuthority(value ?? "checkpoint")}>
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
