import { useEffect, useMemo, useState, type ComponentProps, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import {
  Brain02Icon,
  CheckmarkCircle02Icon,
  File01Icon,
  FolderDetailsIcon,
  LinkSquare02Icon,
  XVariableCircleIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useNavigate } from "react-router-dom";
import { callMcpTool } from "@/api/mcpClient";
import type { EpicListOutputSchema, ProposeAdrShowOutput } from "@/api/generated/mcp-tools.gen";
import { InlineError } from "@/components/InlineError";
import { relativeTime } from "@/components/memory/memoryUtils";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { pulseProposalListQueryOptions, type PulseProposalSummary } from "@/lib/pulseProposals";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";

type ProposalSummary = PulseProposalSummary;

type ProposalDetail = NonNullable<ProposeAdrShowOutput["adr"]>;
type EpicShell = EpicListOutputSchema.EpicModel;
type FilterValue = "all" | "epic" | "architectural" | "task-spike";
type ReviewMode = "accept" | "reject" | null;
type ProposalActionResult = {
  tone: "success" | "error";
  title: string;
  description: string;
  epic?: EpicShell | null;
};

const FILTERS: Array<{ value: FilterValue; label: string }> = [
  { value: "all", label: "All" },
  { value: "epic", label: "Epic-shaped" },
  { value: "architectural", label: "Architectural" },
  { value: "task-spike", label: "Task/Spike" },
];

function normalizeWorkShape(workShape?: string): string {
  return (workShape ?? "architectural").toLowerCase();
}

function matchesFilter(item: ProposalSummary, filter: FilterValue): boolean {
  const workShape = normalizeWorkShape(item.work_shape);
  switch (filter) {
    case "all":
      return true;
    case "epic":
      return workShape === "epic";
    case "architectural":
      return workShape === "architectural";
    case "task-spike":
      return workShape === "task" || workShape === "spike";
  }
}

function workShapeLabel(workShape?: string): string {
  switch (normalizeWorkShape(workShape)) {
    case "epic":
      return "Epic-shaped";
    case "task":
      return "Task";
    case "spike":
      return "Spike";
    default:
      return "Architectural";
  }
}

function workShapeBadgeVariant(workShape?: string): "default" | "secondary" | "outline" {
  switch (normalizeWorkShape(workShape)) {
    case "epic":
      return "default";
    case "task":
    case "spike":
      return "secondary";
    default:
      return "outline";
  }
}

async function callProposalAction(toolName: string, args: Record<string, unknown>): Promise<any> {
  return callMcpTool(toolName as never, args as never);
}

export function ArchitectProposalsSection({ projectPath }: { projectPath: string }) {
  const [filter, setFilter] = useState<FilterValue>("all");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const queryClient = useQueryClient();

  const proposalsQuery = useQuery(pulseProposalListQueryOptions(projectPath));

  const proposals = proposalsQuery.data ?? [];
  const filteredProposals = useMemo(
    () => proposals.filter((item) => matchesFilter(item, filter)),
    [proposals, filter],
  );

  useEffect(() => {
    if (filteredProposals.length === 0) {
      setSelectedId(null);
      return;
    }
    if (!selectedId) {
      setSelectedId(filteredProposals[0].id);
      return;
    }
    if (!filteredProposals.some((item) => item.id === selectedId)) {
      setSelectedId(filteredProposals[0].id);
    }
  }, [filteredProposals, selectedId]);

  const selectedSummary = filteredProposals.find((item) => item.id === selectedId) ?? null;

  const detailQuery = useQuery({
    queryKey: ["pulse", "architect-proposal", projectPath, selectedSummary?.id],
    queryFn: async () => {
      const response = await callMcpTool("propose_adr_show", {
        project: projectPath,
        id: selectedSummary!.id,
      });
      return response.adr ?? null;
    },
    enabled: !!selectedSummary,
    staleTime: 30_000,
    refetchOnWindowFocus: true,
  });

  return (
    <Card>
      <CardHeader className="gap-3">
        <div className="flex flex-col gap-3 lg:flex-row lg:items-start lg:justify-between">
          <div>
            <CardTitle>Architect Proposals</CardTitle>
            <CardDescription>Review draft ADRs without leaving Pulse.</CardDescription>
          </div>
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            <Badge variant="outline">{proposals.length} pending</Badge>
            {proposalsQuery.isFetching && !proposalsQuery.isLoading ? <span>Refreshing…</span> : null}
          </div>
        </div>
        <Tabs value={filter} onValueChange={(value) => setFilter(value as FilterValue)}>
          <TabsList className="w-full justify-start overflow-x-auto">
            {FILTERS.map((item) => (
              <TabsTrigger key={item.value} value={item.value}>
                {item.label}
              </TabsTrigger>
            ))}
          </TabsList>
        </Tabs>
      </CardHeader>

      <CardContent>
        {proposalsQuery.isLoading ? (
          <LoadingState />
        ) : proposalsQuery.error ? (
          <InlineError
            message={proposalsQuery.error instanceof Error ? proposalsQuery.error.message : "Failed to load proposals."}
            onRetry={() => proposalsQuery.refetch()}
            retrying={proposalsQuery.isFetching}
          />
        ) : proposals.length === 0 ? (
          <EmptyState
            title="No pending architect proposals"
            description="Draft ADRs from architect spikes will appear here when they land in the proposed inbox."
          />
        ) : filteredProposals.length === 0 ? (
          <EmptyState
            title="No proposals match this filter"
            description="Try another segment to review a different work shape."
            action={<Button variant="outline" size="sm" onClick={() => setFilter("all")}>Show all</Button>}
          />
        ) : (
          <div className="grid min-h-[28rem] gap-4 lg:grid-cols-[minmax(18rem,24rem)_minmax(0,1fr)]">
            <div className="min-h-0 rounded-xl border border-border/70 bg-background/30">
              <ScrollArea className="h-[28rem] lg:h-full">
                <div className="space-y-2 p-2">
                  {filteredProposals.map((item) => (
                    <ProposalListItem
                      key={item.id}
                      item={item}
                      selected={item.id === selectedSummary?.id}
                      onSelect={() => setSelectedId(item.id)}
                    />
                  ))}
                </div>
              </ScrollArea>
            </div>
            <ProposalDetailPanel
              projectPath={projectPath}
              proposal={selectedSummary}
              detail={detailQuery.data ?? null}
              loading={detailQuery.isLoading}
              error={detailQuery.error instanceof Error ? detailQuery.error.message : null}
              onRetry={() => detailQuery.refetch()}
              retrying={detailQuery.isFetching}
              onReviewed={async (removedId) => {
                if (removedId === selectedId) {
                  setSelectedId(null);
                }
                await proposalsQuery.refetch();
                await Promise.all([
                  queryClient.invalidateQueries({ queryKey: ["epics"] }),
                  queryClient.invalidateQueries({ queryKey: ["tasks"] }),
                ]);
              }}
            />
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function ProposalListItem({
  item,
  selected,
  onSelect,
}: {
  item: ProposalSummary;
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onSelect}
      className={cn(
        "w-full rounded-lg border px-3 py-3 text-left transition-colors",
        selected
          ? "border-primary/40 bg-primary/5"
          : "border-transparent bg-muted/30 hover:border-border hover:bg-muted/50",
      )}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <p className="line-clamp-2 text-sm font-medium text-foreground">{item.title || item.id}</p>
          <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
            <Badge variant={workShapeBadgeVariant(item.work_shape)}>{workShapeLabel(item.work_shape)}</Badge>
            {item.originating_spike_id ? <span>Spike {item.originating_spike_id}</span> : <span>No spike link</span>}
          </div>
        </div>
        <span className="shrink-0 text-[11px] text-muted-foreground">
          {item.modifiedAt ? relativeTime(item.modifiedAt) : "Draft"}
        </span>
      </div>
    </button>
  );
}

function ProposalDetailPanel({
  projectPath,
  proposal,
  detail,
  loading,
  error,
  onRetry,
  retrying,
  onReviewed,
}: {
  projectPath: string;
  proposal: ProposalSummary | null;
  detail: ProposalDetail | null;
  loading: boolean;
  error: string | null;
  onRetry: () => void;
  retrying: boolean;
  onReviewed: (removedId: string) => Promise<void>;
}) {
  const navigate = useNavigate();
  const [reviewMode, setReviewMode] = useState<ReviewMode>(null);
  const [acceptTitle, setAcceptTitle] = useState("");
  const [createEpic, setCreateEpic] = useState(true);
  const [autoBreakdown, setAutoBreakdown] = useState(true);
  const [rejectReason, setRejectReason] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [actionResult, setActionResult] = useState<ProposalActionResult | null>(null);

  const active = detail ?? proposal;
  const isArchitectural = normalizeWorkShape(active?.work_shape) === "architectural";

  useEffect(() => {
    if (!active) {
      setReviewMode(null);
      setAcceptTitle("");
      setCreateEpic(true);
      setAutoBreakdown(true);
      setRejectReason("");
      setSubmitting(false);
      setActionResult(null);
      return;
    }

    setReviewMode(null);
    setAcceptTitle(active.title || "");
    setCreateEpic(!isArchitectural);
    setAutoBreakdown(true);
    setRejectReason("");
    setSubmitting(false);
    setActionResult(null);
  }, [active?.id, active?.title, isArchitectural]);

  if (!proposal) {
    return (
      <div className="flex min-h-[28rem] items-center justify-center rounded-xl border border-dashed border-border bg-background/20 p-6">
        <div className="max-w-sm text-center text-muted-foreground">
          <HugeiconsIcon icon={Brain02Icon} className="mx-auto mb-3 h-8 w-8 opacity-50" />
          <p className="text-sm">Select a proposal to inspect its summary and full body.</p>
        </div>
      </div>
    );
  }

  if (loading) {
    return <DetailLoadingState />;
  }

  if (error) {
    return <InlineError message={error} onRetry={onRetry} retrying={retrying} />;
  }

  if (!active) {
    return <DetailLoadingState />;
  }

  const activeProposal = active;

  const handleAccept = async () => {
    const title = acceptTitle.trim();
    if (!title) {
      const description = "Accepted proposals need a title override or existing title.";
      setActionResult({ tone: "error", title: "Could not accept proposal", description });
      showToast.error("Could not accept proposal", { description });
      return;
    }

    setSubmitting(true);
    setActionResult(null);

    try {
      const response = await callProposalAction("propose_adr_accept", {
        project: projectPath,
        id: activeProposal.id,
        title,
        create_epic: isArchitectural ? false : createEpic,
        auto_breakdown: autoBreakdown,
      });

      if (response?.error) {
        throw new Error(String(response.error));
      }

      const epic = (response?.epic ?? null) as EpicShell | null;
      const description = epic
        ? `Accepted and created epic ${epic.short_id || epic.id}. Open the board to confirm it appeared.`
        : `Accepted and moved to ${response?.accepted_path ?? "the decisions folder"}.`;

      setActionResult({ tone: "success", title: "Proposal accepted", description, epic });
      showToast.success("Proposal accepted", { description });
      await onReviewed(activeProposal.id);
    } catch (acceptError) {
      const description = acceptError instanceof Error ? acceptError.message : "Failed to accept proposal.";
      setActionResult({ tone: "error", title: "Could not accept proposal", description });
      showToast.error("Could not accept proposal", { description });
    } finally {
      setSubmitting(false);
    }
  };

  const handleReject = async () => {
    const reason = rejectReason.trim();
    if (!reason) {
      const description = "Rejecting a proposal requires a non-empty reason.";
      setActionResult({ tone: "error", title: "Could not reject proposal", description });
      showToast.error("Could not reject proposal", { description });
      return;
    }

    setSubmitting(true);
    setActionResult(null);

    try {
      const response = await callProposalAction("propose_adr_reject", {
        project: projectPath,
        id: activeProposal.id,
        reason,
      });

      if (response?.error || response?.ok === false) {
        throw new Error(String(response?.error ?? "Failed to reject proposal."));
      }

      const description = response?.feedback_target
        ? `Feedback persisted to ${String(response.feedback_target)}.`
        : activeProposal.originating_spike_id
          ? `Feedback persisted to originating spike ${activeProposal.originating_spike_id}.`
          : "Proposal rejected. No originating spike was recorded for feedback threading.";

      setActionResult({ tone: "success", title: "Proposal rejected", description });
      showToast.success("Proposal rejected", { description });
      await onReviewed(activeProposal.id);
    } catch (rejectError) {
      const description = rejectError instanceof Error ? rejectError.message : "Failed to reject proposal.";
      setActionResult({ tone: "error", title: "Could not reject proposal", description });
      showToast.error("Could not reject proposal", { description });
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="min-h-[28rem] overflow-hidden rounded-xl border border-border/70 bg-background/30">
      <ScrollArea className="h-[28rem] lg:h-full">
        <div className="space-y-5 p-5">
          <div className="space-y-3">
            <div className="flex flex-wrap items-start justify-between gap-3">
              <div className="space-y-2">
                <h3 className="text-lg font-semibold text-foreground">{activeProposal.title || activeProposal.id}</h3>
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant={workShapeBadgeVariant(activeProposal.work_shape)}>
                    {workShapeLabel(activeProposal.work_shape)}
                  </Badge>
                  <span className="text-xs text-muted-foreground">
                    {proposal.modifiedAt ? `Updated ${relativeTime(proposal.modifiedAt)}` : "Pending review"}
                  </span>
                </div>
              </div>
            </div>

            <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
              <SummaryTile icon={File01Icon} label="Proposal ID" value={activeProposal.id} mono />
              <SummaryTile
                icon={LinkSquare02Icon}
                label="Originating spike"
                value={activeProposal.originating_spike_id ?? "Unspecified"}
              />
              <SummaryTile icon={FolderDetailsIcon} label="Source" value={activeProposal.path} mono />
              <SummaryTile label="Work shape" value={workShapeLabel(activeProposal.work_shape)} />
            </div>
          </div>

          <Separator />

          <div className="space-y-3">
            <div className="flex flex-wrap items-center gap-2">
              <Button size="sm" onClick={() => setReviewMode(reviewMode === "accept" ? null : "accept")} disabled={submitting}>
                Accept
              </Button>
              <Button
                size="sm"
                variant="outline"
                onClick={() => setReviewMode(reviewMode === "reject" ? null : "reject")}
                disabled={submitting}
              >
                Reject
              </Button>
            </div>

            {reviewMode === "accept" ? (
              <div className="space-y-4 rounded-lg border border-border/70 bg-muted/20 p-4">
                <div className="space-y-2">
                  <Label htmlFor={`accept-title-${activeProposal.id}`}>Title override</Label>
                  <Input
                    id={`accept-title-${activeProposal.id}`}
                    value={acceptTitle}
                    onChange={(event) => setAcceptTitle(event.target.value)}
                    disabled={submitting}
                    placeholder="Accepted ADR title"
                  />
                </div>
                {!isArchitectural ? (
                  <div className="space-y-3 text-sm">
                    <label className="flex items-center gap-2 text-foreground">
                      <input
                        type="checkbox"
                        checked={createEpic}
                        onChange={(event) => setCreateEpic(event.target.checked)}
                        disabled={submitting}
                      />
                      <span>Create epic shell on accept</span>
                    </label>
                    <label className="flex items-center gap-2 text-foreground">
                      <input
                        type="checkbox"
                        checked={autoBreakdown}
                        onChange={(event) => setAutoBreakdown(event.target.checked)}
                        disabled={submitting || !createEpic}
                      />
                      <span>Auto-breakdown created epic</span>
                    </label>
                  </div>
                ) : (
                  <p className="text-sm text-muted-foreground">
                    Architectural proposals are accepted into decisions only; they do not create epic shells.
                  </p>
                )}
                <div className="flex flex-wrap items-center gap-2">
                  <Button size="sm" onClick={handleAccept} disabled={submitting}>
                    {submitting ? "Accepting…" : "Confirm accept"}
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setReviewMode(null)} disabled={submitting}>
                    Cancel
                  </Button>
                </div>
              </div>
            ) : null}

            {reviewMode === "reject" ? (
              <div className="space-y-4 rounded-lg border border-border/70 bg-muted/20 p-4">
                <div className="space-y-2">
                  <Label htmlFor={`reject-reason-${activeProposal.id}`}>Reason</Label>
                  <Textarea
                    id={`reject-reason-${activeProposal.id}`}
                    value={rejectReason}
                    onChange={(event) => setRejectReason(event.target.value)}
                    disabled={submitting}
                    rows={4}
                    placeholder="Explain why this draft is being rejected."
                  />
                  <p className="text-xs text-muted-foreground">
                    This reason is sent with the rejection flow and should be threaded back to the originating spike.
                  </p>
                </div>
                <div className="flex flex-wrap items-center gap-2">
                  <Button size="sm" variant="destructive" onClick={handleReject} disabled={submitting}>
                    {submitting ? "Rejecting…" : "Confirm reject"}
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setReviewMode(null)} disabled={submitting}>
                    Cancel
                  </Button>
                </div>
              </div>
            ) : null}

            {actionResult ? (
              <div
                className={cn(
                  "rounded-lg border p-3 text-sm",
                  actionResult.tone === "success"
                    ? "border-emerald-500/30 bg-emerald-500/10 text-emerald-100"
                    : "border-destructive/40 bg-destructive/10 text-destructive",
                )}
              >
                <div className="flex items-start gap-2">
                  <HugeiconsIcon
                    icon={actionResult.tone === "success" ? CheckmarkCircle02Icon : XVariableCircleIcon}
                    className="mt-0.5 h-4 w-4 shrink-0"
                  />
                  <div className="min-w-0 flex-1 space-y-1">
                    <p className="font-medium">{actionResult.title}</p>
                    <p>{actionResult.description}</p>
                    {actionResult.epic ? (
                      <div className="flex flex-wrap items-center gap-2 pt-1">
                        <Badge variant="secondary">Epic {actionResult.epic.short_id || actionResult.epic.id}</Badge>
                        <Button size="sm" variant="outline" onClick={() => navigate("/kanban")}>
                          Open board
                        </Button>
                      </div>
                    ) : null}
                  </div>
                </div>
              </div>
            ) : null}
          </div>

          <Separator />

          <div className="space-y-2">
            <h4 className="text-sm font-medium text-foreground">Draft body</h4>
            <div className="prose prose-sm max-w-none dark:prose-invert">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>
                {activeProposal.body ?? "No body content available."}
              </ReactMarkdown>
            </div>
          </div>
        </div>
      </ScrollArea>
    </div>
  );
}

function SummaryTile({
  label,
  value,
  mono = false,
  icon,
}: {
  label: string;
  value: string;
  mono?: boolean;
  icon?: ComponentProps<typeof HugeiconsIcon>["icon"];
}) {
  return (
    <div className="rounded-lg border border-border/60 bg-muted/20 p-3">
      <div className="mb-1 flex items-center gap-1.5 text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
        {icon ? <HugeiconsIcon icon={icon} className="h-3.5 w-3.5" /> : null}
        <span>{label}</span>
      </div>
      <p className={cn("break-words text-sm text-foreground", mono && "font-mono text-xs")}>{value}</p>
    </div>
  );
}

function LoadingState() {
  return (
    <div className="grid min-h-[28rem] gap-4 lg:grid-cols-[minmax(18rem,24rem)_minmax(0,1fr)]">
      <div className="space-y-2 rounded-xl border border-border/70 p-2">
        {Array.from({ length: 5 }).map((_, index) => (
          <div key={index} className="rounded-lg border border-transparent p-3">
            <Skeleton className="h-4 w-3/4" />
            <div className="mt-3 flex gap-2">
              <Skeleton className="h-5 w-20 rounded-full" />
              <Skeleton className="h-4 w-24" />
            </div>
          </div>
        ))}
      </div>
      <DetailLoadingState />
    </div>
  );
}

function DetailLoadingState() {
  return (
    <div className="rounded-xl border border-border/70 p-5">
      <Skeleton className="h-7 w-1/2" />
      <div className="mt-3 flex gap-2">
        <Skeleton className="h-5 w-24 rounded-full" />
        <Skeleton className="h-4 w-28" />
      </div>
      <div className="mt-5 grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
        {Array.from({ length: 4 }).map((_, index) => (
          <div key={index} className="rounded-lg border border-border/60 p-3">
            <Skeleton className="h-3 w-20" />
            <Skeleton className="mt-2 h-4 w-full" />
          </div>
        ))}
      </div>
      <div className="mt-5 space-y-2">
        {Array.from({ length: 8 }).map((_, index) => (
          <Skeleton key={index} className={cn("h-4", index === 7 ? "w-2/3" : "w-full")} />
        ))}
      </div>
    </div>
  );
}

function EmptyState({
  title,
  description,
  action,
}: {
  title: string;
  description: string;
  action?: ReactNode;
}) {
  return (
    <div className="flex min-h-[18rem] items-center justify-center rounded-xl border border-dashed border-border bg-background/20 p-6 text-center">
      <div className="max-w-md space-y-3">
        <HugeiconsIcon icon={Brain02Icon} className="mx-auto h-8 w-8 text-muted-foreground/50" />
        <div>
          <p className="text-sm font-medium text-foreground">{title}</p>
          <p className="mt-1 text-sm text-muted-foreground">{description}</p>
        </div>
        {action}
      </div>
    </div>
  );
}
