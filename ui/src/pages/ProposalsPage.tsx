import { useMemo, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { ArrowLeft01Icon, Comment01Icon, Robot01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { callMcpTool } from "@/api/mcpClient";
import { usersQueryOptions } from "@/api/queryOptions";
import { userDisplayName, type OrgUser } from "@/api/users";
import { AcceptanceChecklist } from "@/components/AcceptanceChecklist";
import { BlockRenderer } from "@/components/proposals/blocks/BlockRenderer";
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
import type { Project } from "@/api/server";
import type { Proposal, ProposalFeedback } from "@/api/types";

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
        .sort((a, b) => (a.updated_at < b.updated_at ? 1 : -1)),
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

              return (
                <section key={g.status}>
                  <button
                    type="button"
                    aria-label={`${statusLabel(g.status)} ${g.items.length}`}
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
                            {(p.unresolved_feedback_count ?? 0) > 0 && (
                              <span
                                className="flex shrink-0 items-center gap-1 text-xs text-muted-foreground"
                                title={`${p.unresolved_feedback_count} unresolved comment${
                                  p.unresolved_feedback_count === 1 ? "" : "s"
                                }`}
                              >
                                <HugeiconsIcon icon={Comment01Icon} size={13} />
                                {p.unresolved_feedback_count}
                              </span>
                            )}
                            <AcceptanceProgressBadge
                              criteria={p.acceptance_criteria}
                              className="shrink-0"
                            />
                            <span className="hidden shrink-0 font-mono text-xs text-muted-foreground sm:inline">
                              {p.short_id}
                            </span>
                            <span className="shrink-0 text-xs text-muted-foreground">
                              {relativeTime(p.updated_at)}
                            </span>
                            <UserAvatar
                              user={userFor(p.author_user_id)}
                              className="size-5"
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
  const diffRef = useRef<ProposalDiffHandle>(null);
  const me = useAuthUser();
  const caps = capsFromUser(me);
  const usersQuery = useQuery(usersQueryOptions());
  const authorUser = (usersQuery.data ?? []).find(
    (u: OrgUser) => u.id === proposal.author_user_id
  );
  const isAuthor = !!me && proposal.author_user_id === me.id;
  const canDirectEdit = canEdit(caps, isAuthor);
  const untargeted = useMemo(
    () => projects.filter((p) => !detail.targets.some((t) => t.project_id === p.id)),
    [projects, detail.targets]
  );
  const driftState = proposalDriftState(proposal);

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
        <div className="space-y-2">
          <Label className="text-xs uppercase text-muted-foreground">Spec</Label>
          {proposal.body_format === "mdx" ? (
            <BlockRenderer
              body={proposal.body || ""}
              feedback={detail.feedback}
              proposal={proposal}
              canEdit={canDirectEdit}
              onChanged={onChanged}
            />
          ) : (
            <div className="prose prose-sm max-w-none dark:prose-invert">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>
                {proposal.body || "_No spec body yet._"}
              </ReactMarkdown>
            </div>
          )}
        </div>

        <ProposalHistory detail={detail} />

        <Separator />

        <ProposalSignoffs detail={detail} onChanged={onChanged} />

        <ProposalKickoff detail={detail} onChanged={onChanged} />

        <Separator />

        <FeedbackThread
          proposal={proposal}
          feedback={detail.feedback}
          includeAnchoredFeedback={proposal.body_format !== "mdx"}
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
  blockId,
  includeAnchoredFeedback = false,
  canEdit,
  onChanged,
}: {
  proposal: Proposal;
  feedback: ProposalFeedback[];
  blockId?: string;
  includeAnchoredFeedback?: boolean;
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

  const scopedFeedback = feedback.filter((f) => {
    if (blockId) return f.target_section === blockId;
    if (includeAnchoredFeedback) return true;
    return f.target_section == null;
  });
  const unresolved = scopedFeedback.filter((f) => f.resolved_at == null);
  const resolved = scopedFeedback.filter((f) => f.resolved_at != null);
  const compact = Boolean(blockId);

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
      {f.target_section && !compact && <span>· {f.target_section}</span>}
      <span>· {relativeTime(f.created_at)}</span>
      <CopyButton text={f.body} label="Copy comment" className="ml-auto" />
    </div>
  );

  return (
    <div className={compact ? "space-y-3" : "space-y-4"}>
      <div className="flex items-center justify-between">
        {compact ? (
          <Badge variant="secondary">{scopedFeedback.length}</Badge>
        ) : (
          <Label className="text-xs uppercase text-muted-foreground">Feedback</Label>
        )}
        <div className="flex items-center gap-2">
          {!compact && unresolved.length > 0 && (
            <Badge variant="secondary">{unresolved.length} unresolved</Badge>
          )}
          {canEdit && (
            <Button
              size="sm"
              variant="outline"
              className="gap-1.5"
              onClick={() => startChat(proposal, undefined, undefined, blockId)}
            >
              <HugeiconsIcon icon={Robot01Icon} size={15} />
              Ask djinn
            </Button>
          )}
        </div>
      </div>

      <div className="space-y-3">
        {unresolved.length === 0 && (
          <p
            className={
              compact
                ? "text-xs text-muted-foreground"
                : "text-sm text-muted-foreground"
            }
          >
            {compact
              ? "No open feedback for this block."
              : "No open feedback. Reviewers can leave feedback via the djinn MCP, and you can ask djinn to apply it."}
          </p>
        )}
        {unresolved.map((f) => (
          <div
            key={f.id}
            className={
              compact
                ? "rounded-md border bg-muted/40 p-2"
                : "rounded-md border bg-muted/40 p-3"
            }
          >
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
                  onClick={() => startChat(proposal, f, authorName(f), blockId)}
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
                <div
                  key={f.id}
                  className={
                    compact
                      ? "rounded-md border bg-muted/20 p-2"
                      : "rounded-md border bg-muted/20 p-3"
                  }
                >
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
