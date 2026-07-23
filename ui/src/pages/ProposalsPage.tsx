import { useEffect, useMemo, useRef, useState } from "react";
import { useLocation, useNavigate, useParams } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import {
  Alert02Icon,
  ArrowLeft01Icon,
  Comment01Icon,
  JusticeScale01Icon,
  Robot01Icon,
  TestTube01Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { callMcpTool } from "@/api/mcpClient";
import { usersQueryOptions } from "@/api/queryOptions";
import { userDisplayName, type OrgUser } from "@/api/users";
import { AcceptanceChecklist } from "@/components/AcceptanceChecklist";
import {
  BlockRenderer,
  proposalMarkdownComponents,
} from "@/components/proposals/blocks";
import { AcceptanceProgressBadge } from "@/components/AcceptanceProgressBadge";
import { UserAvatar } from "@/components/UserAvatar";
import { CopyButton } from "@/components/CopyButton";
import { InlineError } from "@/components/InlineError";
import { relativeTime } from "@/components/memory/memoryUtils";
import {
  PROPOSAL_STATUS_META,
  PROPOSAL_STATUS_KEYS,
  isArchivedLike,
  statusLabel,
  type ProposalStatus,
} from "@/components/proposals/proposalStatus";
import { StatusIcon } from "@/components/proposals/StatusIcon";
import {
  ProposalDiff,
  type ProposalDiffHandle,
} from "@/components/proposals/ProposalDiff";
import { ProposalSignoffs } from "@/components/proposals/ProposalSignoffs";
import { ProposalKickoff } from "@/components/proposals/ProposalKickoff";
import { ProposalRefinement } from "@/components/proposals/ProposalRefinement";
import { ReadinessPanel } from "@/components/proposals/ReadinessPanel";
import { ProposalHistory } from "@/components/proposals/ProposalHistory";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { showToast } from "@/lib/toast";
import { useAuthUser } from "@/components/AuthGate";
import { canEdit, capsFromUser } from "@/lib/proposalPermissions";
import { useProjects } from "@/stores/useProjectStore";
import { useStartProposalChat } from "@/components/proposals/useStartProposalChat";
import {
  type ProposalDetail as ProposalDetailData,
  proposalDetailQueryOptions,
  proposalListQueryOptions,
} from "@/lib/proposalQueries";
import { cn } from "@/lib/utils";
import type { Project } from "@/api/server";
import type {
  Proposal,
  ProposalFeedback,
  ProposalLintResult,
  ProposalListSummary,
} from "@/api/types";

/**
 * A tribunal has "engaged" once any refinement/debate activity has touched the
 * proposal — an active round, a converged-and-awaiting state, a needs-evidence
 * park, or any debate round on record. A fresh draft that has never been
 * refined has NOT engaged, so its failing Definition-of-Ready is the expected
 * state and must not be alarmed in red.
 */
function tribunalEngaged(summary: ProposalListSummary): boolean {
  return (
    summary.refinement_active ||
    summary.awaiting_review ||
    summary.current_round > 0 ||
    summary.needs_evidence
  );
}

/**
 * One unified tribunal chip for a list row, resolved as a single state machine
 * (priority order, first match wins):
 *
 *  1. awaiting_review → accent `⚖ Review` — the actionable, human-bottleneck
 *     state; the most prominent thing on the row.
 *  2. needs_evidence → muted `🧪 evidence` — parked on a needs-evidence spike.
 *  3. refinement_active → pulsing `⚖ R{n} running` — a round is in flight.
 *  4. tribunal ran (round > 0), idle, and left blocking objections →
 *     red `⚖ R{n} · {count} open`.
 *  5. otherwise → nothing (the slot stays empty but keeps its width).
 */
function TribunalChip({ summary }: { summary: ProposalListSummary }) {
  const round = summary.current_round || 1;

  if (summary.awaiting_review) {
    return (
      <span
        className="flex items-center gap-0.5 text-xs font-semibold text-amber-500"
        title="Tribunal converged — awaiting your review"
      >
        <HugeiconsIcon icon={JusticeScale01Icon} size={13} />
        Review
      </span>
    );
  }
  if (summary.needs_evidence) {
    return (
      <span
        className="flex items-center gap-0.5 text-xs text-muted-foreground"
        title="Parked on a needs-evidence spike"
      >
        <HugeiconsIcon icon={TestTube01Icon} size={13} />
        evidence
      </span>
    );
  }
  if (summary.refinement_active) {
    return (
      <span
        className="flex items-center gap-0.5 text-xs text-muted-foreground"
        title={`Tribunal refinement running (round ${round})`}
      >
        <HugeiconsIcon
          icon={JusticeScale01Icon}
          size={13}
          className="animate-pulse"
        />
        R{round} running
      </span>
    );
  }
  if (summary.current_round > 0 && summary.unresolved_blocking_count > 0) {
    const n = summary.unresolved_blocking_count;
    return (
      <span
        className="flex items-center gap-0.5 text-xs font-medium text-red-500"
        title={`Tribunal round ${summary.current_round} left ${n} unresolved blocking objection${
          n === 1 ? "" : "s"
        }`}
      >
        <HugeiconsIcon icon={JusticeScale01Icon} size={13} />
        R{summary.current_round} · {n} open
      </span>
    );
  }
  return null;
}

/**
 * Gate readiness as a single dot. Red is RESERVED for "a tribunal engaged and
 * the gate is actually stuck" — the only time an operator needs to act:
 *
 *  - gate_ready → subtle, quiet green dot.
 *  - blocked AND a tribunal has engaged → red dot, the reason in the tooltip.
 *  - blocked but no tribunal ever engaged (a fresh draft naturally failing
 *    Definition of Ready) → neutral hollow dot; the tooltip still explains it,
 *    but there is no red and no "DoR ✗" shouting on the row.
 */
function GateDot({ summary }: { summary: ProposalListSummary }) {
  let dotClass: string;
  let title: string;

  if (summary.gate_ready) {
    dotClass = "bg-emerald-500/50";
    title = "Gate ready";
  } else if (tribunalEngaged(summary)) {
    dotClass = "bg-red-500";
    if (summary.unresolved_blocking_count > 0) {
      const n = summary.unresolved_blocking_count;
      title = `Gate blocked — ${n} unresolved blocking objection${
        n === 1 ? "" : "s"
      }`;
    } else if (!summary.dor_ready) {
      title = "Gate blocked — Definition of Ready not met";
    } else if (summary.needs_evidence) {
      title = "Gate blocked — parked on evidence";
    } else {
      title = "Gate blocked — judge verdict: needs work";
    }
  } else {
    dotClass = "border border-muted-foreground/40 bg-transparent";
    title = "Definition of Ready not yet met";
  }

  return (
    <span className="flex items-center" title={title}>
      <span
        className={cn("inline-block size-2 rounded-full", dotClass)}
        aria-hidden="true"
      />
    </span>
  );
}

/** Render a full proposal as markdown — title + spec + acceptance criteria —
 * so it can be copied into an AI to discuss. */
function proposalAsMarkdown(proposal: Proposal): string {
  const ac = (proposal.acceptance_criteria ?? []).map((c) => {
    const item = typeof c === "string" ? { criterion: c, met: false } : c;
    return `- [${item.met ? "x" : " "}] ${item.criterion}`;
  });
  return [
    `# ${proposal.title}`,
    proposal.body?.trim() || "_No spec body yet._",
    ac.length ? `## Acceptance criteria\n${ac.join("\n")}` : "",
  ]
    .filter(Boolean)
    .join("\n\n");
}

// Statuses a human sets directly. `approved` is reached via sign-offs;
// `building` via graduation. `done` is also exposed below for proposals that
// were implemented externally and should leave active proposal history.
const MANUAL_STATUSES: ProposalStatus[] = [
  "triage",
  "draft",
  "in_review",
  "done",
  "rejected",
  "archived",
  "superseded",
];

const TERMINAL_COLLAPSED_STATUSES = new Set<ProposalStatus>([
  "done",
  "rejected",
]);

const BLOCK_HIGHLIGHT_CLASSES = [
  "ring-2",
  "ring-primary",
  "ring-offset-2",
  "transition-all",
  "duration-1000",
  "rounded-lg",
];

function cssStringEscape(value: string): string {
  if (typeof CSS !== "undefined" && typeof CSS.escape === "function") {
    return CSS.escape(value);
  }
  return value.replace(/[^a-zA-Z0-9_-]/g, (char) => `\\${char}`);
}

function defaultCollapsed(status: ProposalStatus): boolean {
  return TERMINAL_COLLAPSED_STATUSES.has(status);
}

function manualStatusLabel(
  status: ProposalStatus,
  currentStatus: ProposalStatus,
): string {
  if (status === "done" && currentStatus !== "done") {
    return "Mark done (implemented externally)";
  }
  return statusLabel(status);
}

interface ProposalDriftState {
  latestSeq: number;
  reconciledSeq: number;
  hasDrift: boolean;
}

function proposalDriftState(proposal: Proposal): ProposalDriftState | null {
  if (proposal.status !== "building") return null;

  const latestSeq = proposal.latest_revision_seq;
  const reconciledSeq = proposal.last_reconciled_revision_seq;
  const hasPendingReconcile = Boolean(proposal.pending_reconcile);

  if (typeof latestSeq !== "number" || typeof reconciledSeq !== "number") {
    return null;
  }

  return {
    latestSeq,
    reconciledSeq,
    hasDrift: hasPendingReconcile || latestSeq > reconciledSeq,
  };
}

/** Presentation-only projection of the repository lint result for the current head. */
function LatestLintBanner({ lint }: { lint?: ProposalLintResult | null }) {
  const errors = lint?.errors ?? [];
  const warnings = lint?.warnings ?? [];
  const hasErrors = errors.length > 0;

  if (!hasErrors && warnings.length === 0) return null;

  const diagnostics = hasErrors ? [...errors, ...warnings] : warnings;
  const tone = hasErrors
    ? {
        container: "border-destructive/40 bg-destructive/5",
        icon: "text-destructive",
        title: "text-destructive",
        heading: "Proposal integrity errors",
      }
    : {
        container:
          "border-amber-200 bg-amber-50 dark:border-amber-800 dark:bg-amber-950/30",
        icon: "text-amber-600",
        title: "text-amber-800 dark:text-amber-200",
        heading: "Proposal integrity warnings",
      };

  return (
    <section
      aria-label="Latest proposal integrity"
      role="alert"
      className={cn("flex items-start gap-2 rounded-md border p-3", tone.container)}
    >
      <HugeiconsIcon icon={Alert02Icon} size={16} className={cn("mt-0.5 shrink-0", tone.icon)} />
      <div className="min-w-0 space-y-2 text-sm">
        <p className={cn("font-medium", tone.title)}>{tone.heading}</p>
        <ul className="space-y-1.5">
          {diagnostics.map((diagnostic, index) => (
            <li key={`${diagnostic.severity}-${diagnostic.code}-${index}`}>
              <span className="font-mono text-xs">{diagnostic.severity}</span>
              <span className="px-1 text-muted-foreground">·</span>
              <span className="font-mono text-xs">{diagnostic.code}</span>
              <span className="px-1 text-muted-foreground">·</span>
              <span>{diagnostic.message}</span>
              <span className="ml-1 font-mono text-xs text-muted-foreground">
                bytes [{diagnostic.span.start}, {diagnostic.span.end})
              </span>
            </li>
          ))}
        </ul>
      </div>
    </section>
  );
}

export function ProposalsPage() {
  const { id } = useParams<{ id?: string }>();
  if (id) return <ProposalDetailRoute id={id} />;
  return <ProposalsListView />;
}

// ── List ─────────────────────────────────────────────────────────────────────

function ProposalsListView() {
  const navigate = useNavigate();
  const [search, setSearch] = useState("");
  const [showArchived, setShowArchived] = useState(false);
  const [collapsedGroups, setCollapsedGroups] = useState<
    Partial<Record<ProposalStatus, boolean>>
  >({});

  const listQuery = useQuery(
    proposalListQueryOptions({ text: search.trim() || undefined })
  );
  const usersQuery = useQuery(usersQueryOptions());
  const userFor = (id?: string | null) =>
    id ? (usersQuery.data ?? []).find((u: OrgUser) => u.id === id) : undefined;

  const groups = useMemo(() => {
    const visible = (listQuery.data ?? []).filter(
      (p) => showArchived || !isArchivedLike(p.status)
    );
    return PROPOSAL_STATUS_KEYS.map((status) => ({
      status,
      items: visible
        .filter((p) => p.status === status)
        // Awaiting-review rows float to the top of their group (the human is
        // the bottleneck), then the existing recency order (updated_at desc).
        .sort((a, b) => {
          const aw = a.list_summary?.awaiting_review ? 1 : 0;
          const bw = b.list_summary?.awaiting_review ? 1 : 0;
          if (aw !== bw) return bw - aw;
          return a.updated_at < b.updated_at ? 1 : -1;
        }),
    })).filter((g) => g.items.length > 0);
  }, [listQuery.data, showArchived]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex items-center justify-between gap-4 border-b p-4">
        <h1 className="text-lg font-semibold">Proposals</h1>
        <div className="flex items-center gap-3">
          <Input
            placeholder="Search proposals…"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            className="w-64"
          />
          <label className="flex items-center gap-2 whitespace-nowrap text-sm text-muted-foreground">
            <Switch
              checked={showArchived}
              onCheckedChange={setShowArchived}
              aria-label="Show archived"
            />
            Show archived
          </label>
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto">
        {listQuery.isLoading ? (
          <div className="space-y-2 p-4">
            {[0, 1, 2].map((i) => (
              <Skeleton key={i} className="h-10 w-full" />
            ))}
          </div>
        ) : listQuery.isError ? (
          <div className="p-4">
            <InlineError message={(listQuery.error as Error).message} />
          </div>
        ) : groups.length === 0 ? (
          <p className="p-10 text-center text-sm text-muted-foreground">
            No proposals yet.
          </p>
        ) : (
          <div className="pb-10">
            {groups.map((g) => {
              const isCollapsed =
                collapsedGroups[g.status] ?? defaultCollapsed(g.status);
              const listId = `proposal-status-group-${g.status}`;
              const awaitingCount = g.items.filter(
                (p) => p.list_summary?.awaiting_review,
              ).length;
              const ariaLabel =
                awaitingCount > 0
                  ? `${statusLabel(g.status)} ${g.items.length} · ${awaitingCount} awaiting review`
                  : `${statusLabel(g.status)} ${g.items.length}`;

              return (
                <section key={g.status}>
                  <button
                    type="button"
                    aria-label={ariaLabel}
                    aria-expanded={!isCollapsed}
                    aria-controls={listId}
                    onClick={() =>
                      setCollapsedGroups((current) => ({
                        ...current,
                        [g.status]: !(current[g.status] ?? defaultCollapsed(g.status)),
                      }))
                    }
                    className="flex w-full items-center gap-2 bg-muted/40 px-4 py-1.5 text-left text-xs font-medium text-muted-foreground hover:bg-muted/60 focus:outline-none focus:ring-2 focus:ring-ring"
                  >
                    <span
                      className="w-3 text-muted-foreground/70"
                      aria-hidden="true"
                    >
                      {isCollapsed ? "▸" : "▾"}
                    </span>
                    <StatusIcon status={g.status} />
                    <span>{statusLabel(g.status)}</span>
                    <span className="text-muted-foreground/60">{g.items.length}</span>
                    {awaitingCount > 0 && (
                      <span className="font-normal text-amber-500/80">
                        · {awaitingCount} awaiting review
                      </span>
                    )}
                  </button>
                  {!isCollapsed && (
                    <ul id={listId}>
                      {g.items.map((p) => (
                        <li key={p.id}>
                          <button
                            onClick={() => navigate(`/proposals/${p.id}`)}
                            className="flex w-full items-center gap-3 border-b border-border/40 px-4 py-2.5 text-left hover:bg-muted/40"
                          >
                            <StatusIcon status={p.status} />
                            <span className="min-w-0 flex-1 truncate text-sm">
                              {p.title}
                            </span>
                            {/* Right rail — fixed-width slots so every row's
                                columns line up down the list. Empty slots keep
                                their width so nothing shifts. */}
                            <span className="flex w-28 shrink-0 items-center justify-start">
                              {p.list_summary && (
                                <TribunalChip summary={p.list_summary} />
                              )}
                            </span>
                            <span className="flex w-4 shrink-0 justify-center">
                              {p.list_summary && (
                                <GateDot summary={p.list_summary} />
                              )}
                            </span>
                            <span className="flex w-9 shrink-0 justify-end">
                              {(p.unresolved_feedback_count ?? 0) > 0 && (
                                <span
                                  className="flex items-center gap-1 text-xs text-muted-foreground"
                                  title={`${p.unresolved_feedback_count} unresolved comment${
                                    p.unresolved_feedback_count === 1 ? "" : "s"
                                  }`}
                                >
                                  <HugeiconsIcon icon={Comment01Icon} size={13} />
                                  {p.unresolved_feedback_count}
                                </span>
                              )}
                            </span>
                            <span className="flex w-11 shrink-0 justify-end">
                              <AcceptanceProgressBadge met={p.ac_met ?? 0} total={p.ac_total ?? 0} />
                            </span>
                            <span className="hidden w-10 shrink-0 text-right font-mono text-xs text-muted-foreground sm:inline-block">
                              {p.short_id}
                            </span>
                            <span className="w-16 shrink-0 whitespace-nowrap text-right text-xs text-muted-foreground">
                              {relativeTime(p.updated_at)}
                            </span>
                            <UserAvatar
                              user={userFor(p.author_user_id)}
                              className="size-5 shrink-0"
                            />
                          </button>
                        </li>
                      ))}
                    </ul>
                  )}
                </section>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}

// ── Detail route ─────────────────────────────────────────────────────────────

function ProposalDetailRoute({ id }: { id: string }) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const projects = useProjects();
  const detailQuery = useQuery(proposalDetailQueryOptions(id));

  const invalidate = () => queryClient.invalidateQueries({ queryKey: ["proposals"] });

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex items-center gap-2 border-b px-4 py-2">
        <Button variant="ghost" size="sm" onClick={() => navigate("/proposals")} className="gap-1.5">
          <HugeiconsIcon icon={ArrowLeft01Icon} size={16} />
          Proposals
        </Button>
      </div>
      {detailQuery.isLoading ? (
        <div className="space-y-4 p-6">
          <Skeleton className="h-8 w-2/3" />
          <Skeleton className="h-40 w-full" />
        </div>
      ) : detailQuery.isError ? (
        <div className="p-6">
          <InlineError message={(detailQuery.error as Error).message} />
        </div>
      ) : detailQuery.data?.proposal ? (
        <ProposalDetailView
          detail={detailQuery.data}
          projects={projects}
          onChanged={invalidate}
        />
      ) : (
        <p className="p-10 text-center text-sm text-muted-foreground">Proposal not found.</p>
      )}
    </div>
  );
}

function ProposalDetailView({
  detail,
  projects,
  onChanged,
}: {
  detail: ProposalDetailData;
  projects: Project[];
  onChanged: () => void;
}) {
  const proposal = detail.proposal as Proposal;
  const location = useLocation();
  const diffRef = useRef<ProposalDiffHandle>(null);
  const specContainerRef = useRef<HTMLDivElement>(null);
  const highlightTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const me = useAuthUser();
  const caps = capsFromUser(me);
  const usersQuery = useQuery(usersQueryOptions());
  const authorUser = (usersQuery.data ?? []).find(
    (u: OrgUser) => u.id === proposal.author_user_id
  );
  const isAuthor = !!me && proposal.author_user_id === me.id;
  const canDirectEdit = canEdit(caps, isAuthor);
  const feedback = detail.feedback;
  const untargeted = useMemo(
    () => projects.filter((p) => !detail.targets.some((t) => t.project_id === p.id)),
    [projects, detail.targets]
  );
  const driftState = proposalDriftState(proposal);

  useEffect(() => {
    const blockId = new URLSearchParams(
      window.location.search || location.search,
    ).get("block");
    if (!blockId) return undefined;

    const specContainer = specContainerRef.current;
    if (!specContainer) return undefined;

    const target = specContainer.querySelector<HTMLElement>(
      `[id="${cssStringEscape(blockId)}"]`,
    );
    if (!target) return undefined;

    if (highlightTimeoutRef.current) {
      clearTimeout(highlightTimeoutRef.current);
    }

    target.scrollIntoView({ behavior: "smooth", block: "center" });
    target.classList.add(...BLOCK_HIGHLIGHT_CLASSES);

    highlightTimeoutRef.current = setTimeout(() => {
      target.classList.remove(...BLOCK_HIGHLIGHT_CLASSES);
      highlightTimeoutRef.current = null;
    }, 3000);

    return () => {
      if (highlightTimeoutRef.current) {
        clearTimeout(highlightTimeoutRef.current);
        highlightTimeoutRef.current = null;
      }
      target.classList.remove(...BLOCK_HIGHLIGHT_CLASSES);
    };
  }, [location.search, proposal.id, proposal.body, proposal.body_format]);

  const openRevisionDiff = () => {
    diffRef.current?.open();
    diffRef.current?.focus();
  };

  const run = async (fn: () => Promise<{ error?: string }>, ok: string) => {
    try {
      const res = await fn();
      if (res.error) throw new Error(res.error);
      showToast.success(ok);
      onChanged();
    } catch (e) {
      showToast.error("Action failed", { description: (e as Error).message });
    }
  };

  return (
    <div className="min-h-0 flex-1 overflow-y-auto">
      <div className="mx-auto max-w-3xl space-y-6 p-6">
        {/* Header */}
        <div className="flex items-start justify-between gap-4">
          <div className="min-w-0">
            <h2 className="text-xl font-semibold">{proposal.title}</h2>
            <div className="mt-1 flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
              <span className="font-mono">{proposal.short_id}</span>
              {proposal.author_user_id && (
                <>
                  <span>·</span>
                  <span className="flex items-center gap-1.5">
                    <UserAvatar user={authorUser} className="size-4" />
                    {authorUser ? userDisplayName(authorUser) : "unknown"}
                  </span>
                </>
              )}
              <span>·</span>
              <span>updated {relativeTime(proposal.updated_at)}</span>
              {driftState && (
                <>
                  <span>·</span>
                  {driftState.hasDrift ? (
                    <button
                      type="button"
                      onClick={openRevisionDiff}
                      className="inline-flex items-center gap-1.5 rounded-full border border-amber-500/40 bg-amber-500/10 px-2 py-0.5 text-xs font-medium text-amber-700 transition hover:bg-amber-500/15 focus:outline-none focus:ring-2 focus:ring-amber-500/40 dark:text-amber-300"
                    >
                      <StatusIcon status="in_review" size={12} />
                      {`spec at rev ${driftState.latestSeq} · build reconciled to rev ${driftState.reconciledSeq} · reconciling…`}
                    </button>
                  ) : (
                    <span className="inline-flex items-center gap-1.5 text-muted-foreground">
                      spec at rev {driftState.latestSeq} · build reconciled
                    </span>
                  )}
                </>
              )}
            </div>
          </div>
          <div className="flex items-center gap-2">
            <CopyButton
              text={proposalAsMarkdown(proposal)}
              label="Copy spec"
              showLabel
            />
            <Select
              value={proposal.status}
              onValueChange={(status) =>
                typeof status === "string" &&
                run(
                  () => callMcpTool("proposal_update", { id: proposal.id, status }),
                  `Status → ${statusLabel(status)}`
                )
              }
            >
              <SelectTrigger className="w-[150px]" aria-label="Proposal status">
                <span className="flex items-center gap-2">
                  <StatusIcon status={proposal.status} />
                  {statusLabel(proposal.status)}
                </span>
              </SelectTrigger>
              <SelectContent>
                {MANUAL_STATUSES.filter(
                  (s) =>
                    s !== "done" ||
                    !PROPOSAL_STATUS_META[proposal.status as ProposalStatus]
                      ?.terminal,
                ).map((s) => (
                  <SelectItem key={s} value={s}>
                    <span className="flex items-center gap-2">
                      <StatusIcon status={s} />
                      {manualStatusLabel(s, proposal.status as ProposalStatus)}
                    </span>
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        </div>

        <LatestLintBanner lint={detail.latest_lint} />

        {driftState && (
          <ProposalDiff
            ref={diffRef}
            revisions={detail.revisions}
            baseSeq={driftState.reconciledSeq}
            headSeq={driftState.latestSeq}
          />
        )}

        {/* Targets */}
        <div className="space-y-2">
          <Label className="text-xs uppercase text-muted-foreground">Target projects</Label>
          <div className="flex flex-wrap items-center gap-2">
            {detail.targets.length === 0 && (
              <span className="text-sm text-muted-foreground">None yet</span>
            )}
            {detail.targets.map((t) => (
              <span
                key={t.project_id}
                className="flex items-center gap-1 rounded-full border px-3 py-1 text-xs"
              >
                {t.project_name ?? t.project_path ?? t.project_id}
                {t.role === "reference" && <span className="text-muted-foreground">(ref)</span>}
                <button
                  className="ml-1 text-muted-foreground hover:text-destructive"
                  onClick={() =>
                    run(
                      () =>
                        callMcpTool("proposal_remove_target", {
                          id: proposal.id,
                          project: t.project_id,
                        }),
                      "Target removed"
                    )
                  }
                >
                  ×
                </button>
              </span>
            ))}
            {untargeted.length > 0 && (
              <Select
                value=""
                onValueChange={(pid) =>
                  typeof pid === "string" &&
                  run(
                    () => callMcpTool("proposal_add_target", { id: proposal.id, project: pid }),
                    "Target added"
                  )
                }
              >
                <SelectTrigger className="h-7 w-[150px] text-xs" aria-label="Add target">
                  <SelectValue placeholder="+ Add target" />
                </SelectTrigger>
                <SelectContent>
                  {untargeted.map((p) => (
                    <SelectItem key={p.id} value={p.id}>
                      {p.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            )}
          </div>
        </div>

        {/* Acceptance criteria — read-only checklist (no clickable/met state). */}
        {(proposal.acceptance_criteria?.length ?? 0) > 0 && (
          <div className="space-y-2">
            <Label className="text-xs uppercase text-muted-foreground">Acceptance criteria</Label>
            <AcceptanceChecklist criteria={proposal.acceptance_criteria} />
          </div>
        )}

        {/* Spec body — read-only; editing happens via djinn in chat. */}
        <div className="space-y-2" ref={specContainerRef} data-testid="proposal-spec">
          <Label className="text-xs uppercase text-muted-foreground">Spec</Label>
          {proposal.body_format === "mdx" ? (
            <BlockRenderer body={proposal.body || ""} />
          ) : (
            <div className="prose prose-sm max-w-none dark:prose-invert">
              <ReactMarkdown
                remarkPlugins={[remarkGfm]}
                components={proposalMarkdownComponents}
              >
                {proposal.body || "_No spec body yet._"}
              </ReactMarkdown>
            </div>
          )}
        </div>

        {/* Tribunal: refinement kickoff / status ribbon, and — once converged —
            the review card (verdict / spec diff / debate trail tabs plus the
            human accept / another-round / reject actions). */}
        <ProposalRefinement
          proposalId={proposal.id}
          status={detail.refinement}
          authorUserId={proposal.author_user_id}
          signoffUserIds={detail.signoffs.map((signoff) => signoff.user_id)}
          gateStatus={detail.gate_status}
          debateTrail={detail.debate_trail}
          revisions={detail.revisions}
          canStart={
            (proposal.status === "draft" || proposal.status === "in_review") &&
            !detail.refinement?.active
          }
          onChanged={onChanged}
        />

        {/* Readiness gate: per-condition checklist (DoR, judge verdict,
            unresolved blocking debate entries, evidence spike) rendered
            straight from gate_status — no client-side recomputation. */}
        <ReadinessPanel
          proposalId={proposal.id}
          gateStatus={detail.gate_status}
          refinement={detail.refinement}
          debateTrail={detail.debate_trail}
          onChanged={onChanged}
        />

        <Separator />

        <ProposalSignoffs detail={detail} onChanged={onChanged} />

        <ProposalKickoff detail={detail} onChanged={onChanged} />

        <Separator />

        <ProposalHistory detail={detail} />

        <Separator />

        {/* Human feedback thread. The tribunal debate trail (objections /
            rebuttals / verdicts) now lives in the Tribunal review card above;
            this thread is for human discussion only. */}
        <FeedbackThread
          proposal={proposal}
          feedback={feedback}
          canEdit={canDirectEdit}
          onChanged={onChanged}
        />
      </div>
    </div>
  );
}

// ── Feedback ─────────────────────────────────────────────────────────────────

export function FeedbackThread({
  proposal,
  feedback,
  canEdit,
  onChanged,
}: {
  proposal: Proposal;
  feedback: ProposalFeedback[];
  canEdit: boolean;
  onChanged: () => void;
}) {
  const [showResolved, setShowResolved] = useState(false);
  const usersQuery = useQuery(usersQueryOptions());
  const userFor = (id?: string | null) =>
    id ? (usersQuery.data ?? []).find((u: OrgUser) => u.id === id) : undefined;
  const startChat = useStartProposalChat();

  const authorName = (f: ProposalFeedback) => {
    if (f.author_kind === "ai") return f.author_model ?? "ai";
    const u = userFor(f.author_user_id);
    return u ? userDisplayName(u) : "reviewer";
  };

  // Dismiss = resolve with no revision (no spec change). Applying feedback runs
  // through djinn in chat, which resolves it with the revision it landed in.
  const dismiss = async (id: string) => {
    try {
      const res = await callMcpTool("proposal_feedback_resolve", { id });
      if (res.error) throw new Error(res.error);
      onChanged();
    } catch (e) {
      showToast.error("Failed to dismiss", { description: (e as Error).message });
    }
  };

  const unresolved = feedback.filter((f) => f.resolved_at == null);
  const resolved = feedback.filter((f) => f.resolved_at != null);

  const authorHeader = (f: ProposalFeedback) => (
    <div className="mb-1 flex items-center gap-2 text-xs text-muted-foreground">
      {f.author_kind === "ai" ? (
        <Badge variant="secondary">{f.author_model ?? "ai"}</Badge>
      ) : (
        <span className="flex items-center gap-1.5">
          <UserAvatar user={userFor(f.author_user_id)} className="size-4" />
          <span className="font-medium text-foreground">{authorName(f)}</span>
        </span>
      )}
      <span>· {relativeTime(f.created_at)}</span>
      <CopyButton text={f.body} label="Copy comment" className="ml-auto" />
    </div>
  );

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <Label className="text-xs uppercase text-muted-foreground">Feedback</Label>
        <div className="flex items-center gap-2">
          {unresolved.length > 0 && (
            <Badge variant="secondary">{unresolved.length} unresolved</Badge>
          )}
          {canEdit && (
            <Button
              size="sm"
              variant="outline"
              className="gap-1.5"
              onClick={() => startChat(proposal)}
            >
              <HugeiconsIcon icon={Robot01Icon} size={15} />
              Ask djinn
            </Button>
          )}
        </div>
      </div>

      <div className="space-y-3">
        {unresolved.length === 0 && (
          <p className="text-sm text-muted-foreground">
            No open feedback. Reviewers can leave feedback via the djinn MCP, and
            you can ask djinn to apply it.
          </p>
        )}
        {unresolved.map((f) => (
          <div key={f.id} className="rounded-md border bg-muted/40 p-3">
            {authorHeader(f)}
            <div className="prose prose-sm max-w-none dark:prose-invert">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>{f.body}</ReactMarkdown>
            </div>
            {canEdit && (
              <div className="mt-2 flex gap-2">
                <Button
                  size="sm"
                  variant="outline"
                  className="gap-1.5"
                  onClick={() => startChat(proposal, f, authorName(f))}
                >
                  <HugeiconsIcon icon={Robot01Icon} size={15} />
                  Address with djinn
                </Button>
                <Button size="sm" variant="ghost" onClick={() => dismiss(f.id)}>
                  Dismiss
                </Button>
              </div>
            )}
          </div>
        ))}
      </div>

      {resolved.length > 0 && (
        <div className="space-y-2">
          <button
            type="button"
            onClick={() => setShowResolved((v) => !v)}
            className="text-xs text-muted-foreground hover:text-foreground"
          >
            {showResolved ? "Hide" : "Show"} resolved ({resolved.length})
          </button>
          {showResolved && (
            <div className="space-y-3 opacity-70">
              {resolved.map((f) => (
                <div key={f.id} className="rounded-md border bg-muted/20 p-3">
                  {authorHeader(f)}
                  <div className="prose prose-sm max-w-none dark:prose-invert">
                    <ReactMarkdown remarkPlugins={[remarkGfm]}>{f.body}</ReactMarkdown>
                  </div>
                  <p className="mt-2 text-xs text-muted-foreground">
                    {f.resolved_revision_seq != null
                      ? `Addressed in revision ${f.resolved_revision_seq}`
                      : "Dismissed"}
                  </p>
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
