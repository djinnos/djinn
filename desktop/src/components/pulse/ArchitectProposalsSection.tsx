import { useEffect, useMemo, useState, type ComponentProps, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import {
  Brain02Icon,
  File01Icon,
  FolderDetailsIcon,
  LinkSquare02Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { callMcpTool } from "@/api/mcpClient";
import type { ProposeAdrShowOutput } from "@/api/generated/mcp-tools.gen";
import { InlineError } from "@/components/InlineError";
import { relativeTime } from "@/components/memory/memoryUtils";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { pulseProposalListQueryOptions, type PulseProposalSummary } from "@/lib/pulseProposals";
import { cn } from "@/lib/utils";

type ProposalSummary = PulseProposalSummary;

type ProposalDetail = NonNullable<ProposeAdrShowOutput["adr"]>;

type FilterValue = "all" | "epic" | "architectural" | "task-spike";

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

export function ArchitectProposalsSection({ projectPath }: { projectPath: string }) {
  const [filter, setFilter] = useState<FilterValue>("all");
  const [selectedId, setSelectedId] = useState<string | null>(null);

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
              proposal={selectedSummary}
              detail={detailQuery.data ?? null}
              loading={detailQuery.isLoading}
              error={detailQuery.error instanceof Error ? detailQuery.error.message : null}
              onRetry={() => detailQuery.refetch()}
              retrying={detailQuery.isFetching}
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
  proposal,
  detail,
  loading,
  error,
  onRetry,
  retrying,
}: {
  proposal: ProposalSummary | null;
  detail: ProposalDetail | null;
  loading: boolean;
  error: string | null;
  onRetry: () => void;
  retrying: boolean;
}) {
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

  const active = detail ?? proposal;

  return (
    <div className="min-h-[28rem] overflow-hidden rounded-xl border border-border/70 bg-background/30">
      <ScrollArea className="h-[28rem] lg:h-full">
        <div className="space-y-5 p-5">
          <div className="space-y-3">
            <div className="flex flex-wrap items-start justify-between gap-3">
              <div className="space-y-2">
                <h3 className="text-lg font-semibold text-foreground">{active.title || active.id}</h3>
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant={workShapeBadgeVariant(active.work_shape)}>
                    {workShapeLabel(active.work_shape)}
                  </Badge>
                  <span className="text-xs text-muted-foreground">
                    {proposal.modifiedAt ? `Updated ${relativeTime(proposal.modifiedAt)}` : "Pending review"}
                  </span>
                </div>
              </div>
            </div>

            <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
              <SummaryTile icon={File01Icon} label="Proposal ID" value={active.id} mono />
              <SummaryTile
                icon={LinkSquare02Icon}
                label="Originating spike"
                value={active.originating_spike_id ?? "Unspecified"}
              />
              <SummaryTile icon={FolderDetailsIcon} label="Source" value={active.path} mono />
              <SummaryTile label="Work shape" value={workShapeLabel(active.work_shape)} />
            </div>
          </div>

          <Separator />

          <div className="space-y-2">
            <h4 className="text-sm font-medium text-foreground">Draft body</h4>
            <div className="prose prose-sm max-w-none dark:prose-invert">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>
                {active.body ?? "No body content available."}
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
