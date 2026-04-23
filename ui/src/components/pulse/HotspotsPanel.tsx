import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { HugeiconsIcon } from "@hugeicons/react";
import { ArrowDown01Icon, ArrowRight01Icon } from "@hugeicons/core-free-icons";
import { Card, CardHeader, CardTitle, CardDescription, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { callMcpTool } from "@/api/mcpClient";
import { cn } from "@/lib/utils";
import {
  parseRanked,
  parseFileGroups,
  truncatePathLeft,
  type RankedNode,
} from "./pulseTypes";

interface HotspotsPanelProps {
  projectPath: string;
}

function formatScore(n: number, max: number): string {
  if (max <= 0) return "—";
  return `${Math.round((n / max) * 100)}%`;
}

function NeighborsDrilldown({
  projectPath,
  nodeKey,
  selfFile,
}: {
  projectPath: string;
  nodeKey: string;
  selfFile: string;
}) {
  // Roll up incoming edges by file: the raw neighbor list is every SCIP
  // symbol that references this file (local bindings, params, etc.), which
  // isn't useful for "what depends on this file". `group_by: "file"` answers
  // the question we actually care about.
  const { data, isLoading, error } = useQuery({
    queryKey: ["pulse", "neighbors", projectPath, nodeKey, "incoming", "by-file"],
    queryFn: async () => {
      const raw = await callMcpTool("code_graph", {
        project_path: projectPath,
        operation: "neighbors",
        key: nodeKey,
        direction: "incoming",
        group_by: "file",
        limit: 8,
      });
      return parseFileGroups(raw);
    },
    staleTime: 60_000,
  });

  if (isLoading) {
    return (
      <div className="mt-2 space-y-1.5 pl-8">
        {Array.from({ length: 3 }).map((_, i) => (
          <Skeleton key={i} className="h-3 w-3/4" />
        ))}
      </div>
    );
  }

  if (error) {
    return (
      <p className="mt-2 pl-8 text-xs text-muted-foreground">
        Couldn&apos;t load referrers.
      </p>
    );
  }

  // Drop self-references — a file trivially "references" itself via its own
  // contained symbols, which isn't interesting.
  const groups = (data ?? []).filter((g) => g.file !== selfFile);

  if (groups.length === 0) {
    return (
      <p className="mt-2 pl-8 text-xs text-muted-foreground">
        No other files reference this one.
      </p>
    );
  }

  const max = Math.max(...groups.map((g) => g.occurrence_count), 1);
  return (
    <div className="mt-2 space-y-1.5 pl-8">
      <p className="text-[11px] font-medium text-muted-foreground">
        Files that reference this
      </p>
      {groups.map((g) => {
        const pct = Math.round((g.occurrence_count / max) * 100);
        return (
          <div key={g.file} className="space-y-0.5">
            <div className="flex items-center justify-between gap-2 text-xs">
              <span
                className="truncate font-mono text-foreground/80"
                title={g.file}
                dir="rtl"
              >
                {truncatePathLeft(g.file, 64)}
              </span>
              <span className="shrink-0 tabular-nums text-muted-foreground">
                {pct}%
              </span>
            </div>
            <div className="h-1 w-full overflow-hidden rounded bg-muted">
              <div
                className="h-full bg-foreground/30"
                style={{ width: `${pct}%` }}
              />
            </div>
          </div>
        );
      })}
    </div>
  );
}

function HotspotRow({
  index,
  node,
  maxRank,
  expanded,
  onToggle,
  projectPath,
}: {
  index: number;
  node: RankedNode;
  maxRank: number;
  expanded: boolean;
  onToggle: () => void;
  projectPath: string;
}) {
  const pct = maxRank > 0 ? (node.page_rank / maxRank) * 100 : 0;
  const label = node.display_name || node.key;
  return (
    <div className="rounded-lg px-2 py-2 transition-colors hover:bg-muted/40">
      <button
        type="button"
        onClick={onToggle}
        className="flex w-full items-center gap-3 text-left"
      >
        <span className="w-5 shrink-0 text-right text-xs tabular-nums text-muted-foreground">
          {index + 1}
        </span>
        <HugeiconsIcon
          icon={expanded ? ArrowDown01Icon : ArrowRight01Icon}
          className="h-3 w-3 shrink-0 text-muted-foreground"
        />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span
              className="min-w-0 flex-1 truncate font-mono text-xs text-foreground"
              title={label}
              dir="rtl"
            >
              {truncatePathLeft(label)}
            </span>
            <span className="shrink-0 tabular-nums text-xs text-muted-foreground">
              {formatScore(node.page_rank, maxRank)}
            </span>
          </div>
          <div className="mt-1 h-1 w-full overflow-hidden rounded bg-muted">
            <div
              className={cn(
                "h-full bg-emerald-400/70 transition-all",
                expanded && "bg-emerald-400"
              )}
              style={{ width: `${pct}%` }}
            />
          </div>
        </div>
      </button>
      {expanded && (
        <NeighborsDrilldown
          projectPath={projectPath}
          nodeKey={node.key}
          selfFile={node.display_name || node.key}
        />
      )}
    </div>
  );
}

export function HotspotsPanel({ projectPath }: HotspotsPanelProps) {
  const [expanded, setExpanded] = useState<string | null>(null);

  const { data, isLoading, error, refetch, isFetching } = useQuery({
    queryKey: ["pulse", "ranked", projectPath, "file"],
    queryFn: async () => {
      const raw = await callMcpTool("code_graph", {
        project_path: projectPath,
        operation: "ranked",
        kind_filter: "file",
        limit: 20,
      });
      return parseRanked(raw);
    },
    staleTime: 60_000,
  });

  // Backend (`code_graph ranked`) already applies project exclusions;
  // just slice the first 10 for the render.
  const top = (data ?? []).slice(0, 10);
  const maxRank = top.length ? Math.max(...top.map((r) => r.page_rank)) : 0;

  return (
    <Card>
      <CardHeader>
        <CardTitle>Hotspots</CardTitle>
        <CardDescription>Top files by structural centrality.</CardDescription>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="space-y-3">
            {Array.from({ length: 6 }).map((_, i) => (
              <div key={i} className="flex items-center gap-3">
                <Skeleton className="h-3 w-4" />
                <Skeleton className="h-3 flex-1" />
                <Skeleton className="h-3 w-8" />
              </div>
            ))}
          </div>
        ) : error ? (
          <div className="flex items-center justify-between gap-3 text-sm">
            <p className="text-muted-foreground">Couldn&apos;t load hotspots.</p>
            <Button size="sm" variant="outline" onClick={() => refetch()} disabled={isFetching}>
              Retry
            </Button>
          </div>
        ) : top.length === 0 ? (
          <p className="text-sm text-muted-foreground">No ranked files yet.</p>
        ) : (
          <div className="space-y-1">
            {top.map((node, i) => (
              <HotspotRow
                key={node.key}
                index={i}
                node={node}
                maxRank={maxRank}
                expanded={expanded === node.key}
                onToggle={() =>
                  setExpanded((prev) => (prev === node.key ? null : node.key))
                }
                projectPath={projectPath}
              />
            ))}
          </div>
        )}
      </CardContent>
    </Card>
  );
}
