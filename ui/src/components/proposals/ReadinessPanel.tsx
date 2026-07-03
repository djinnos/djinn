import { useCallback, useMemo, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  Alert02Icon,
  AlertCircleIcon,
  ArrowDown01Icon,
  CancelCircleIcon,
  CheckmarkCircle02Icon,
  Comment01Icon,
  JusticeScale01Icon,
  MinusSignIcon,
  Shield01Icon,
} from "@hugeicons/core-free-icons";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ConfirmButton } from "@/components/ConfirmButton";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { relativeTime } from "@/components/memory/memoryUtils";
import { callMcpTool } from "@/api/mcpClient";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import type {
  ProposalDebateTrailRow,
  ProposalGateStatus,
  ProposalRefinementStatus,
} from "@/api/types";
import { BlockMarkdown } from "./blocks/BlockShell";
import { classifyRefinementEvidence } from "./refinementEvidenceStatus";

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

type RowState = "pass" | "fail" | "na";

const ROW_MARKER: Record<
  RowState,
  { icon: typeof CheckmarkCircle02Icon; cls: string }
> = {
  pass: {
    icon: CheckmarkCircle02Icon,
    cls: "text-green-600 dark:text-green-400",
  },
  fail: { icon: CancelCircleIcon, cls: "text-destructive" },
  na: { icon: MinusSignIcon, cls: "text-muted-foreground" },
};

function GateRow({
  state,
  label,
  detail,
  action,
  children,
}: {
  state: RowState;
  label: string;
  detail: React.ReactNode;
  action?: React.ReactNode;
  children?: React.ReactNode;
}) {
  const marker = ROW_MARKER[state];
  return (
    <div className="space-y-1">
      <div className="flex items-start gap-2 text-sm">
        <HugeiconsIcon
          icon={marker.icon}
          size={15}
          className={cn("mt-0.5 shrink-0", marker.cls)}
          aria-hidden
        />
        <span className="w-40 shrink-0 font-medium">{label}</span>
        <span className="min-w-0 flex-1 text-muted-foreground">{detail}</span>
        {action}
      </div>
      {children}
    </div>
  );
}

/** One unresolved blocking debate entry, resolved against the trail. */
function BlockingEntryCard({
  row,
  superseded,
  onResolve,
  resolving,
}: {
  row: ProposalDebateTrailRow;
  superseded: boolean;
  onResolve: (id: string) => void;
  resolving: boolean;
}) {
  const [open, setOpen] = useState(false);
  const icon = row.kind === "verdict" ? JusticeScale01Icon : Comment01Icon;
  return (
    <li className="rounded-md border bg-background/60 p-2">
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <HugeiconsIcon
          icon={icon}
          size={13}
          className="shrink-0 text-destructive"
        />
        <span className="font-medium uppercase tracking-wide text-foreground">
          {row.agent_role || row.kind}
        </span>
        <span className="text-muted-foreground">round {row.round}</span>
        <span className="font-mono text-muted-foreground">
          vs rev {row.against_revision_seq}
        </span>
        {row.blocking && (
          <Badge variant="destructive" className="text-[10px]">
            blocking
          </Badge>
        )}
        {superseded && (
          <Badge variant="outline" className="text-[10px]">
            superseded
          </Badge>
        )}
        {row.resolved_at && (
          <span className="inline-flex items-center gap-1 text-green-600 dark:text-green-400">
            <HugeiconsIcon icon={CheckmarkCircle02Icon} size={12} />
            resolved
          </span>
        )}
        <span className="ml-auto shrink-0 text-muted-foreground">
          {relativeTime(row.created_at)}
        </span>
      </div>
      {open ? (
        <div className="mt-1.5">
          <BlockMarkdown>{row.body}</BlockMarkdown>
        </div>
      ) : (
        <p className="mt-1 line-clamp-2 text-xs text-muted-foreground">
          {row.body.replace(/\s+/g, " ").trim() || "(no content)"}
        </p>
      )}
      <div className="mt-1.5 flex items-center gap-2">
        <button
          type="button"
          aria-expanded={open}
          onClick={() => setOpen((v) => !v)}
          className="text-[11px] text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
        >
          {open ? "Collapse" : "Expand"}
        </button>
        {!row.resolved_at && (
          <ConfirmButton
            title="Resolve blocking entry?"
            description="Mark this debate entry resolved so it no longer blocks readiness. This is an audited action."
            confirmLabel="Resolve entry"
            variant="outline"
            size="sm"
            disabled={resolving}
            onConfirm={() => onResolve(row.id)}
          >
            {resolving ? "Resolving…" : "Resolve"}
          </ConfirmButton>
        )}
      </div>
    </li>
  );
}

/**
 * Readiness gate for a proposal, rendered as a per-condition checklist.
 *
 * Each backend gate condition (Definition of Ready, judge verdict, unresolved
 * blocking debate entries, needs-evidence spike) is one row with a pass/fail/na
 * marker; the overall badge comes straight from `gate_status.ready`. The panel
 * renders backend-provided status only — it does NOT recompute readiness logic
 * client-side. The judge's full reasoning markdown is rendered once, in the
 * Tribunal review card; this checklist only references the verdict outcome.
 */
export function ReadinessPanel({
  proposalId,
  gateStatus,
  refinement,
  debateTrail = [],
  onChanged,
}: {
  proposalId?: string;
  gateStatus: ProposalGateStatus | null;
  refinement: ProposalRefinementStatus | null;
  debateTrail?: ProposalDebateTrailRow[];
  onChanged?: () => void;
}) {
  const [overrideReason, setOverrideReason] = useState("");
  const [overrideBusy, setOverrideBusy] = useState(false);
  const [showOverride, setShowOverride] = useState(false);
  const [showBlocking, setShowBlocking] = useState(false);
  const [resolvingId, setResolvingId] = useState<string | null>(null);

  const isReady = gateStatus?.ready ?? true;
  const showOverrideControls = Boolean(
    proposalId &&
      gateStatus &&
      !gateStatus.ready &&
      !gateStatus.human_override_active,
  );

  // Latest judge verdict (highest revision it was written against) for the
  // round/revision metadata the judge row references.
  const latestVerdict = useMemo(() => {
    const verdicts = debateTrail.filter((r) => r.kind === "verdict");
    if (verdicts.length === 0) return null;
    return [...verdicts].sort(
      (a, b) =>
        a.against_revision_seq - b.against_revision_seq ||
        a.created_at.localeCompare(b.created_at),
    )[verdicts.length - 1];
  }, [debateTrail]);
  const maxVerdictRev = latestVerdict?.against_revision_seq ?? -1;

  // Resolve the gate's unresolved blocking ids to real trail rows. Ids that
  // aren't present in the trail fall back to a raw line so we never render worse
  // than the old uuid list.
  const blockingRows = useMemo(() => {
    const ids = gateStatus?.unresolved_blocking_ids ?? [];
    const byId = new Map(debateTrail.map((r) => [r.id, r] as const));
    return ids.map((id) => ({ id, row: byId.get(id) ?? null }));
  }, [gateStatus?.unresolved_blocking_ids, debateTrail]);

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
      setShowOverride(false);
      onChanged?.();
    } catch (e) {
      showToast.error("Failed to record human override", {
        description: (e as Error).message,
      });
    } finally {
      setOverrideBusy(false);
    }
  }, [gateStatus, proposalId, overrideReason, onChanged]);

  const handleResolve = useCallback(
    async (id: string) => {
      setResolvingId(id);
      try {
        const res = await callMcpTool("proposal_debate_resolve", { id });
        if (res.error) throw new Error(res.error);
        showToast.success("Blocking entry resolved");
        onChanged?.();
      } catch (e) {
        showToast.error("Failed to resolve entry", {
          description: (e as Error).message,
        });
      } finally {
        setResolvingId(null);
      }
    },
    [onChanged],
  );

  // If there's no gate status and no refinement, don't render anything.
  if (!gateStatus && !refinement) return null;

  // Judge row state + detail.
  const judgeHasVerdict = Boolean(gateStatus?.judge_verdict_body || latestVerdict);
  const judgeState: RowState = !gateStatus
    ? "na"
    : gateStatus.judge_needs_work
      ? "fail"
      : judgeHasVerdict
        ? "pass"
        : "na";
  const judgeRound = latestVerdict?.round;
  const judgeDetail = !gateStatus
    ? "no verdict yet"
    : gateStatus.judge_needs_work
      ? `needs-work${judgeRound ? ` (round ${judgeRound})` : ""}`
      : judgeHasVerdict
        ? `approve / ready${judgeRound ? ` (round ${judgeRound})` : ""}`
        : "no verdict yet";

  // Blocking-entries row.
  const blockingCount = gateStatus?.unresolved_blocking_count ?? 0;
  const blockingState: RowState = blockingCount > 0 ? "fail" : "pass";

  // Evidence-spike classification — prefer status-level evidence, fall back to
  // the gate-level payload so the readiness panel still renders when the status
  // object is not yet populated.
  const evidenceDisplay = classifyRefinementEvidence(
    refinement,
    gateStatus?.needs_evidence,
  );
  const evidence = evidenceDisplay.evidence;

  return (
    <div className="space-y-3 rounded-md border p-3">
      {/* Header */}
      <div className="flex items-center justify-between">
        <Label className="text-xs uppercase text-muted-foreground">
          Readiness gate
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

      {gateStatus && (
        <div className="space-y-2">
          {/* Definition of Ready */}
          <GateRow
            state={gateStatus.dor_ready ? "pass" : "fail"}
            label="Definition of Ready"
            detail={
              gateStatus.dor_ready
                ? "all checks pass"
                : `${gateStatus.dor_failures.length} check${
                    gateStatus.dor_failures.length === 1 ? "" : "s"
                  } failing`
            }
          >
            {gateStatus.dor_failures.length > 0 && (
              <ul className="ml-[26px] space-y-0.5 text-xs text-muted-foreground">
                {gateStatus.dor_failures.map((f, i) => (
                  <li key={i}>
                    <span className="font-medium">{checkLabel(f.check)}</span>:{" "}
                    {f.message}
                  </li>
                ))}
              </ul>
            )}
          </GateRow>

          {/* Judge verdict */}
          <GateRow
            state={judgeState}
            label="Judge verdict"
            detail={judgeDetail}
          />

          {/* Blocking debate entries */}
          <GateRow
            state={blockingState}
            label="Blocking debate entries"
            detail={
              blockingCount > 0
                ? `${blockingCount} unresolved`
                : "none unresolved"
            }
            action={
              blockingCount > 0 ? (
                <button
                  type="button"
                  aria-expanded={showBlocking}
                  onClick={() => setShowBlocking((v) => !v)}
                  className="inline-flex shrink-0 items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
                >
                  {showBlocking ? "hide" : "show"}
                  <HugeiconsIcon
                    icon={ArrowDown01Icon}
                    size={12}
                    className={cn(
                      "transition-transform",
                      showBlocking && "rotate-180",
                    )}
                  />
                </button>
              ) : undefined
            }
          >
            {showBlocking && blockingCount > 0 && (
              <ul className="ml-[26px] space-y-1.5">
                {blockingRows.map(({ id, row }) =>
                  row ? (
                    <BlockingEntryCard
                      key={id}
                      row={row}
                      superseded={
                        row.kind === "verdict" &&
                        row.against_revision_seq < maxVerdictRev
                      }
                      onResolve={handleResolve}
                      resolving={resolvingId === id}
                    />
                  ) : (
                    <li
                      key={id}
                      className="flex items-start gap-1.5 rounded-md border bg-background/60 p-2 text-xs text-destructive"
                    >
                      <HugeiconsIcon
                        icon={Alert02Icon}
                        size={12}
                        className="mt-0.5 shrink-0"
                      />
                      <span className="break-all font-mono">{id}</span>
                    </li>
                  ),
                )}
              </ul>
            )}
          </GateRow>

          {/* Evidence spike — renders distinct states for awaiting, received,
              failed, and paused/frozen evidence phases. */}
          <GateRow
            state={
              evidence
                ? evidenceDisplay.kind === "evidence_received"
                  ? "pass"
                  : "fail"
                : "na"
            }
            label="Evidence spike"
            detail={
              evidence
                ? evidenceDisplay.kind === "evidence_failed"
                  ? `evidence failed — spike ${evidence.spike_short_id}${evidenceDisplay.failureReason ? ` (${evidenceDisplay.failureReason})` : ""}`
                  : evidenceDisplay.kind === "evidence_received"
                    ? `evidence received — spike ${evidence.spike_short_id}`
                    : `awaiting evidence — spike ${evidence.spike_short_id} (${evidence.spike_status})`
                : "none required"
            }
          >
            {evidence && (
              <p className="ml-[26px] text-xs text-muted-foreground">
                {evidenceDisplay.claimSummary ?? evidence.claim}
              </p>
            )}
          </GateRow>

          <p className="border-t pt-2 text-xs text-muted-foreground">
            Ready when every row clears.
          </p>
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

      {/* Override — collapsed to a quiet link; only when blocked and no
          override is already active. */}
      {showOverrideControls && !showOverride && (
        <button
          type="button"
          onClick={() => setShowOverride(true)}
          className="text-xs text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
        >
          Something wrong with the gate itself? Record override…
        </button>
      )}

      {showOverrideControls && showOverride && (
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
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              variant="destructive"
              disabled={overrideBusy || !overrideReason.trim()}
              onClick={handleOverride}
            >
              {overrideBusy ? "Recording…" : "Override DoR and proceed"}
            </Button>
            <Button
              size="sm"
              variant="ghost"
              disabled={overrideBusy}
              onClick={() => setShowOverride(false)}
            >
              Cancel
            </Button>
          </div>
        </div>
      )}

      {/* Needs-evidence spike parking note — renders distinct copy for
          awaiting, received, failed, and paused/frozen evidence phases. */}
      {evidenceDisplay.kind === "evidence_failed" && evidence && (
        <div className="flex items-start gap-2 rounded-md border border-destructive/40 bg-destructive/5 p-2">
          <HugeiconsIcon
            icon={CancelCircleIcon}
            size={14}
            className="mt-0.5 shrink-0 text-destructive"
          />
          <div className="space-y-0.5 text-xs">
            <p className="font-medium text-destructive">
              Evidence failed: {evidence.spike_short_id}
            </p>
            <p className="text-muted-foreground">
              {evidenceDisplay.claimSummary}
            </p>
            {evidenceDisplay.failureReason && (
              <p className="text-muted-foreground">
                Reason: {evidenceDisplay.failureReason}
              </p>
            )}
            <p className="text-muted-foreground">
              Refinement is blocked on failed or missing evidence findings.
            </p>
          </div>
        </div>
      )}

      {evidenceDisplay.kind === "awaiting_evidence" && evidence && (
        <div className="flex items-start gap-2 rounded-md border border-amber-200 bg-amber-50 p-2 dark:border-amber-800 dark:bg-amber-950/30">
          <HugeiconsIcon
            icon={AlertCircleIcon}
            size={14}
            className="mt-0.5 shrink-0 text-amber-600"
          />
          <div className="space-y-0.5 text-xs">
            <p className="font-medium text-amber-800 dark:text-amber-200">
              Awaiting evidence: {evidence.spike_short_id}
            </p>
            <p className="text-muted-foreground">
              {evidenceDisplay.claimSummary}
            </p>
            <p className="text-muted-foreground">
              Spike task: {evidence.spike_short_id} — {evidence.spike_status}
            </p>
            <p className="font-mono text-[10px] text-muted-foreground">
              {evidence.spike_task_id}
            </p>
          </div>
        </div>
      )}

      {evidenceDisplay.kind === "evidence_received" && evidence && (
        <div className="flex items-start gap-2 rounded-md border border-amber-200 bg-amber-50 p-2 dark:border-amber-800 dark:bg-amber-950/30">
          <HugeiconsIcon
            icon={AlertCircleIcon}
            size={14}
            className="mt-0.5 shrink-0 text-amber-600"
          />
          <div className="space-y-0.5 text-xs">
            <p className="font-medium text-amber-800 dark:text-amber-200">
              Evidence received
            </p>
            <p className="text-muted-foreground">
              {evidenceDisplay.claimSummary}
            </p>
            <p className="text-muted-foreground">
              Spike task: {evidence.spike_short_id} — {evidence.spike_status}
            </p>
          </div>
        </div>
      )}

      {evidenceDisplay.kind === "paused_frozen" && (
        <div className="flex items-start gap-2 rounded-md border border-slate-200 bg-slate-50 p-2 dark:border-slate-800 dark:bg-slate-950/30">
          <HugeiconsIcon
            icon={AlertCircleIcon}
            size={14}
            className="mt-0.5 shrink-0 text-muted-foreground"
          />
          <div className="space-y-0.5 text-xs">
            <p className="font-medium text-foreground">Paused or frozen</p>
            <p className="text-muted-foreground">
              Refinement is paused or frozen manually. Evidence work is on hold
              until the proposal is unfrozen.
            </p>
            {evidence && (
              <>
                <p className="text-muted-foreground">
                  {evidenceDisplay.claimSummary}
                </p>
                <p className="text-muted-foreground">
                  Spike task: {evidence.spike_short_id} — {evidence.spike_status}
                </p>
              </>
            )}
          </div>
        </div>
      )}

      {/* Active-refinement note — only while the tribunal is still running and
          no evidence lifecycle state overrides the ordinary in-progress copy. */}
      {evidenceDisplay.showInProgress && (
        <p className="text-xs text-muted-foreground">
          Autonomous tribunal in progress: Adversary, Advocate, and Judge refine
          the spec automatically. You will be asked to accept or reject the
          converged result.
        </p>
      )}
    </div>
  );
}
