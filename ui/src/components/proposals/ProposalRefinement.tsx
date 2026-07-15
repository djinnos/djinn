import { useState, useCallback, useEffect, useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  CancelCircleIcon,
  CheckmarkCircle02Icon,
} from "@hugeicons/core-free-icons";
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { showToast } from "@/lib/toast";
import type {
  ProposalDebateTrailRow,
  ProposalGateStatus,
  ProposalRefinementStatus,
} from "@/api/types";
import type { ProposalHistoryEntry } from "@/lib/proposalQueries";
import { BlockMarkdown } from "./blocks/BlockShell";
import { DiffView } from "./DiffView";
import { ProposalDebateTrail } from "./ProposalDebateTrail";
import { refinementDiffBodies } from "./refinementDiff";
import { classifyRefinementEvidence } from "./refinementEvidenceStatus";

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
 * Unified Tribunal section: refinement kickoff, a status ribbon reconciling the
 * tribunal state with the readiness gate, and — once the autonomous tribunal
 * converges — a single review card with the verdict, the spec diff, and the
 * round-by-round debate trail behind tabs, plus the human accept / another-round
 * / reject actions.
 *
 * Refinement runs as an autonomous tribunal (Adversary → Advocate → Judge,
 * looping). The human is no longer a per-revision approver: when the tribunal
 * converges it parks for a single human accept/reject of the full refined
 * result (`awaiting_review`).
 */
export function ProposalRefinement({
  proposalId,
  status,
  authorUserId,
  signoffUserIds = [],
  gateStatus,
  debateTrail = [],
  revisions = [],
  canStart,
  onChanged,
}: {
  proposalId: string;
  status: ProposalRefinementStatus | null;
  /** The proposal author plus sign-off users are the only valid run owners. */
  authorUserId?: string | null;
  signoffUserIds?: string[];
  gateStatus?: ProposalGateStatus | null;
  debateTrail?: ProposalDebateTrailRow[];
  revisions?: ProposalHistoryEntry[];
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
  // User the tribunal runs are attributed to (task owner + model scope).
  // Like build kickoff, only the proposal author and sign-off participants
  // are eligible; never manufacture an admin or system fallback.
  const participants = useMemo(() => {
    const ids = new Set<string>();
    if (authorUserId) ids.add(authorUserId);
    signoffUserIds.forEach((id) => ids.add(id));
    return Array.from(ids);
  }, [authorUserId, signoffUserIds]);
  const defaultOwner =
    me?.id && participants.includes(me.id) ? me.id : (participants[0] ?? "");
  const [owner, setOwner] = useState<string>(defaultOwner);
  useEffect(() => {
    setOwner((current) => {
      // User and participant data can resolve after the initial render.
      if (me?.id && participants.includes(me.id)) return me.id;
      return participants.includes(current) ? current : defaultOwner;
    });
  }, [defaultOwner, me?.id, participants]);

  // Optional feedback the human attaches when accepting/rejecting the result.
  // Required to demand another tribunal round (sent as the request reason).
  const [feedback, setFeedback] = useState<string>("");
  const feedbackBlank = !feedback.trim();

  const diff = useMemo(
    () => refinementDiffBodies(revisions, status?.snapshot_revision_seq),
    [revisions, status?.snapshot_revision_seq],
  );

  const handleResolve = useCallback(
    async (decision: "accept" | "reject") => {
      setBusy(true);
      try {
        const res = await callMcpTool("proposal_refinement_resolve", {
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
    const round = status.current_round ?? undefined;
    const headSeq = diff?.headSeq;

    // Evidence lifecycle state — use the shared classifier from task 2grd
    // for consistent display logic across renderers.
    const display = classifyRefinementEvidence(status, gateStatus?.needs_evidence);

    // Evidence object and spike helpers from the classifier.
    const evidence = display.evidence;

    // Status ribbon: one line reconciling the tribunal state.
    const ribbon =
      display.kind === "awaiting_evidence" && evidence
        ? `Awaiting evidence — spike ${display.spikeShortId} (${evidence.spike_status})`
        : display.kind === "evidence_failed" && evidence
          ? `Evidence failed — spike ${display.spikeShortId}${display.failureReason ? ` (${display.failureReason})` : ""}`
          : display.kind === "paused_frozen"
            ? "Paused — waiting on manual gate"
            : display.kind === "evidence_received"
              ? "Evidence received — waiting on refinement to resume"
              : status.active && status.awaiting_review
                ? `Converged${round ? ` after ${round} ${round === 1 ? "round" : "rounds"}` : ""} — awaiting your review`
                : status.active
                  ? `Tribunal running${round ? ` — round ${round}` : ""}`
                  : `Stopped — ${stopReasonLabel(status.stop_reason ?? "")}`;

    return (
      <div className="space-y-3 rounded-md border p-3">
        <div className="flex items-center justify-between">
          <Label className="text-xs uppercase text-muted-foreground">
            Tribunal
          </Label>
          <div className="flex items-center gap-2">
            {status.active && status.awaiting_review ? (
              <Badge variant="secondary" className="text-xs">
                Awaiting review
              </Badge>
            ) : display.kind === "awaiting_evidence" ? (
              <Badge variant="secondary" className="text-xs">
                Awaiting evidence
              </Badge>
            ) : display.kind === "evidence_failed" ? (
              <Badge variant="destructive" className="text-xs">
                Evidence failed
              </Badge>
            ) : display.kind === "paused_frozen" ? (
              <Badge variant="secondary" className="text-xs">
                Paused
              </Badge>
            ) : display.kind === "evidence_received" ? (
              <Badge variant="secondary" className="text-xs">
                Evidence received
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
            {gateStatus &&
              (gateStatus.ready ? (
                <Badge variant="default" className="gap-1 text-xs">
                  <HugeiconsIcon icon={CheckmarkCircle02Icon} size={12} />
                  Ready
                </Badge>
              ) : (
                <Badge variant="destructive" className="gap-1 text-xs">
                  <HugeiconsIcon icon={CancelCircleIcon} size={12} />
                  Blocked
                </Badge>
              ))}
          </div>
        </div>

        {/* Status ribbon */}
        <p className="text-sm font-medium text-foreground">{ribbon}</p>

        <div className="flex flex-wrap gap-3 text-xs text-muted-foreground">
          {status.owner_user_id && (
            <span className="flex items-center gap-1.5">
              owned by
              <UserAvatar user={userFor(status.owner_user_id)} className="size-4" />
              {nameFor(status.owner_user_id)}
            </span>
          )}
          <span>
            Entries{" "}
            <span className="font-mono font-medium text-foreground">
              {status.total_entries}
            </span>
          </span>
          {status.dry_rounds > 0 && (
            <span>
              Dry rounds{" "}
              <span className="font-mono font-medium text-foreground">
                {status.dry_rounds}
              </span>
            </span>
          )}
        </div>

        {/* Stopped — let the user run a fresh tribunal from here. */}
        {!status.active && (
          <div className="space-y-2 border-t pt-2">
            <div className="flex flex-wrap items-center gap-2">
              <span className="text-sm">Owner</span>
              <Select value={owner} onValueChange={setOwner}>
                <SelectTrigger className="h-8 w-[200px] text-sm">
                  <SelectValue placeholder="Pick a participant">
                    {owner ? (
                      <span className="flex items-center gap-2">
                        <UserAvatar user={userFor(owner)} className="size-4" />
                        {nameFor(owner)}
                      </span>
                    ) : undefined}
                  </SelectValue>
                </SelectTrigger>
                <SelectContent>
                  {participants.map((id) => (
                    <SelectItem key={id} value={id}>
                      <span className="flex items-center gap-2">
                        <UserAvatar user={userFor(id)} className="size-4" />
                        {nameFor(id)}
                      </span>
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <Button size="sm" disabled={busy || !owner} onClick={handleStart}>
                {busy ? "Starting…" : "Restart refinement"}
              </Button>
            </div>
            <p className="text-xs text-muted-foreground">
              Runs a fresh Adversary → Advocate → Judge tribunal from the current
              spec.
            </p>
          </div>
        )}

        {/* Converged — single human review of the full refined result, with the
            verdict, the spec diff, and the debate trail behind tabs. */}
        {status.active && status.awaiting_review && (
          <div className="space-y-3 rounded-md border border-primary/40 bg-primary/5 p-3">
            <div className="flex flex-wrap items-baseline justify-between gap-2">
              <Label className="text-sm font-medium">Review the result</Label>
              <span className="text-xs text-muted-foreground">
                {round ? `round ${round}` : null}
                {round && headSeq != null ? " · " : null}
                {headSeq != null ? `revision ${headSeq}` : null}
              </span>
            </div>

            <Tabs defaultValue="verdict" className="gap-3">
              <TabsList className="w-fit">
                <TabsTrigger value="verdict">Verdict</TabsTrigger>
                <TabsTrigger value="diff">
                  Spec diff
                  {diff?.fromSeq != null && diff.headSeq != null
                    ? ` rev ${diff.fromSeq} → ${diff.headSeq}`
                    : ""}
                </TabsTrigger>
                <TabsTrigger value="trail">Debate trail</TabsTrigger>
              </TabsList>

              {/* Verdict — the judge's reasoning markdown, rendered ONCE on the
                  whole page (the readiness checklist only references it). */}
              <TabsContent value="verdict">
                <div className="rounded-md bg-muted/40 p-2 text-sm text-foreground">
                  <BlockMarkdown>
                    {judgeSummaryText(
                      gateStatus?.judge_verdict_body || status.judge_summary,
                    )}
                  </BlockMarkdown>
                </div>
              </TabsContent>

              {/* Spec diff — pre-refinement snapshot → converged head. */}
              <TabsContent value="diff">
                {diff ? (
                  <DiffView before={diff.before} after={diff.after} />
                ) : (
                  <p className="text-sm text-muted-foreground">
                    No spec diff available.
                  </p>
                )}
              </TabsContent>

              {/* Debate trail — round-by-round objections/rebuttals/verdicts. */}
              <TabsContent value="trail">
                <ProposalDebateTrail trail={debateTrail} />
              </TabsContent>
            </Tabs>

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
                Accept refined spec
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
                Reject and revert
              </Button>
            </div>
            <p className="text-xs text-muted-foreground">
              Accept keeps the refined spec. Send feedback reruns the tribunal
              with your notes. Reject reverts to your original spec.
            </p>
          </div>
        )}

        {/* Awaiting evidence — shows claim, spike details, and investigation context. */}
        {display.kind === "awaiting_evidence" && evidence && (
          <div className="space-y-2 rounded-md border border-amber-200 bg-amber-50 p-3 dark:border-amber-800 dark:bg-amber-950/30">
            <Label className="text-xs uppercase text-muted-foreground">
              Awaiting evidence
            </Label>
            <p className="text-sm text-muted-foreground">
              The tribunal has parked refinement pending evidence from spike{" "}
              <span className="font-mono font-medium">
                {evidence.spike_short_id}
              </span>
              .
            </p>
            <p className="text-xs text-muted-foreground">
              Claim: {evidence.claim}
            </p>
            {evidence.question && (
              <p className="text-xs text-muted-foreground">
                Question: {evidence.question}
              </p>
            )}
            {evidence.target_subsystem && (
              <p className="text-xs text-muted-foreground">
                Target: {evidence.target_subsystem}
              </p>
            )}
            {evidence.spec_unknown_anchor && (
              <p className="text-xs text-muted-foreground">
                Unknown: {evidence.spec_unknown_anchor}
              </p>
            )}
          </div>
        )}

        {/* Evidence failed — blocking state with remediation-oriented copy. */}
        {display.kind === "evidence_failed" && evidence && (
          <div className="space-y-2 rounded-md border border-destructive/40 bg-destructive/5 p-3">
            <Label className="text-xs uppercase text-destructive">
              Evidence failed
            </Label>
            <p className="text-sm text-muted-foreground">
              The evidence spike{" "}
              <span className="font-mono font-medium">
                {evidence.spike_short_id}
              </span>{" "}
              {evidence.failure_reason === "spike_force_closed"
                ? "was force-closed"
                : evidence.failure_reason === "malformed_findings"
                  ? "submitted malformed findings"
                  : evidence.failure_reason === "spike_cancelled"
                    ? "was cancelled"
                    : evidence.failure_reason === "spike_errored"
                      ? "encountered an error"
                      : "failed"}
              {evidence.failure_reason
                ? ` (${evidence.failure_reason})`
                : ""}
              .
            </p>
            <p className="text-xs text-muted-foreground">
              Review the spike activity, address the underlying issue, and
              restart evidence collection to unblock refinement.
            </p>
            <p className="text-xs text-muted-foreground">
              Claim: {evidence.claim}
            </p>
          </div>
        )}

        {/* PausedOrFrozen — manual gate copy takes precedence, but evidence
            context is still visible to explain why the proposal is blocked. */}
        {display.kind === "paused_frozen" && (
          <div className="space-y-2 border-t pt-2">
            <Label className="text-xs uppercase text-muted-foreground">
              Paused — waiting on manual gate
            </Label>
            <p className="text-xs text-muted-foreground">
              Refinement is paused until a manual gate is cleared.
              {evidence
                ? " Evidence has been collected and is ready for review."
                : ""}
            </p>
            {evidence && (
              <div className="rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
                Evidence spike: {evidence.spike_short_id} (
                {evidence.spike_status})
                {evidence.claim && (
                  <> — claim: {evidence.claim}</>
                )}
              </div>
            )}
          </div>
        )}

        {/* Evidence received — evidence is available, refinement can resume. */}
        {display.kind === "evidence_received" && (
          <div className="space-y-2 border-t pt-2">
            <Label className="text-xs uppercase text-muted-foreground">
              Evidence received
            </Label>
            <p className="text-xs text-muted-foreground">
              Evidence has been collected and the spike findings are available.
              {evidence?.spike_short_id && (
                <>
                  {" "}
                  Spike:{" "}
                  <span className="font-mono font-medium">
                    {evidence.spike_short_id}
                  </span>
                </>
              )}
            </p>
          </div>
        )}

        {/* In-progress: tribunal still running autonomously. Only shown for
            non-evidence active states — evidence states render their own
            body sections above. Uses the shared classifier's showInProgress
            flag to determine visibility. */}
        {status.active && !status.awaiting_review && display.showInProgress && display.kind !== "evidence_received" && (
          <div className="space-y-1 border-t pt-2">
            <Label className="text-xs uppercase text-muted-foreground">
              Refinement in progress
            </Label>
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
        <span className="text-sm">Owner</span>
        <Select
          value={owner}
          onValueChange={(v) => typeof v === "string" && setOwner(v)}
        >
          <SelectTrigger className="h-8 w-[200px] text-sm">
            {/* Render the resolved name explicitly: `owner` is set
                programmatically, so Radix never captures the selected item's
                text and SelectValue would otherwise fall back to the raw id. */}
            <SelectValue placeholder="Pick a participant">
              {owner ? (
                <span className="flex items-center gap-2">
                  <UserAvatar user={userFor(owner)} className="size-4" />
                  {nameFor(owner)}
                </span>
              ) : undefined}
            </SelectValue>
          </SelectTrigger>
          <SelectContent>
            {participants.map((id) => (
              <SelectItem key={id} value={id}>
                <span className="flex items-center gap-2">
                  <UserAvatar user={userFor(id)} className="size-4" />
                  {nameFor(id)}
                </span>
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Button size="sm" disabled={busy || !owner} onClick={handleStart}>
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
