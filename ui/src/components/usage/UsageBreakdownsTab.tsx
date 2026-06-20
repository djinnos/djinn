import { type ReactNode, useMemo, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  ArrowDown01Icon,
  ArrowRight01Icon,
  ArrowUp01Icon,
} from "@hugeicons/core-free-icons";
import type {
  UsageAnalyticsResponse,
  UsageBreakdownRow,
  UsageModelSplit,
  UsageTimeSeriesPoint,
} from "@/api/analytics";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { cn } from "@/lib/utils";
import {
  EM_DASH,
  formatAverageReopens,
  formatBucket,
  formatCompactNumber,
  formatCurrency,
  formatInteger,
  formatPercent,
  totalTokens,
} from "./usageFormatters";

type BreakdownKind = "user" | "project" | "proposal" | "task";
type SortKey = "name" | "cost" | "cost_per_task" | "success_rate" | "avg_reopens" | "task_count" | "tokens";
type SortDirection = "asc" | "desc";

interface BreakdownConfig {
  kind: BreakdownKind;
  value: string;
  label: string;
  rows: UsageBreakdownRow[];
}

const DEFAULT_SORT: Record<BreakdownKind, SortKey> = {
  user: "cost",
  project: "cost",
  proposal: "task_count",
  task: "cost",
};

export function UsageBreakdownsTab({ data }: { data: UsageAnalyticsResponse }) {
  const breakdowns = data.breakdowns;
  const configs: BreakdownConfig[] = [
    { kind: "user", value: "user", label: "By User", rows: breakdowns?.by_user ?? [] },
    { kind: "project", value: "project", label: "By Project", rows: breakdowns?.by_project ?? [] },
    { kind: "proposal", value: "proposal", label: "By Proposal", rows: breakdowns?.by_proposal ?? [] },
    { kind: "task", value: "task", label: "By Task", rows: breakdowns?.by_task ?? [] },
  ];

  return (
    <Tabs defaultValue="user" className="gap-4">
      <TabsList className="w-fit">
        {configs.map((config) => (
          <TabsTrigger key={config.value} value={config.value}>
            {config.label}
          </TabsTrigger>
        ))}
      </TabsList>
      {configs.map((config) => (
        <TabsContent key={config.value} value={config.value}>
          <BreakdownTable config={config} />
        </TabsContent>
      ))}
    </Tabs>
  );
}

function BreakdownTable({ config }: { config: BreakdownConfig }) {
  const [sortKey, setSortKey] = useState<SortKey>(DEFAULT_SORT[config.kind]);
  const [sortDirection, setSortDirection] = useState<SortDirection>("desc");
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});

  const sortedRows = useMemo(
    () => sortRows(config.rows, sortKey, sortDirection),
    [config.rows, sortDirection, sortKey],
  );

  const toggleSort = (next: SortKey) => {
    if (next === sortKey) {
      setSortDirection((current) => (current === "asc" ? "desc" : "asc"));
    } else {
      setSortKey(next);
      setSortDirection(next === "name" ? "asc" : "desc");
    }
  };

  if (config.rows.length === 0) {
    return (
      <div className="rounded-lg border border-dashed border-border bg-card/50 px-4 py-8 text-center">
        <p className="text-sm font-medium text-foreground">No {config.label.toLowerCase()} rows</p>
        <p className="mt-1 text-sm text-muted-foreground">
          Rows will appear when usage exists for the selected filters.
        </p>
      </div>
    );
  }

  return (
    <section className="rounded-lg border border-border bg-card">
      <div className="border-b border-border px-4 py-3">
        <h2 className="text-sm font-medium text-foreground">{config.label}</h2>
        <p className="mt-1 text-xs text-muted-foreground">
          Sort by any metric. Expand rows with detail to inspect per-model split and time-series buckets.
        </p>
      </div>
      <div className="overflow-x-auto">
        <table className="w-full min-w-[1040px] text-left text-sm">
          <thead className="border-b border-border text-xs text-muted-foreground">
            <tr>
              <th className="w-10 px-3 py-3 font-medium" aria-label="Expand row" />
              <SortableHeader label={config.kind === "task" ? "Task" : config.kind === "proposal" ? "Proposal" : "Name"} sortKey="name" activeKey={sortKey} direction={sortDirection} onSort={toggleSort} />
              <SortableHeader label="Spend" sortKey="cost" activeKey={sortKey} direction={sortDirection} onSort={toggleSort} />
              <SortableHeader label="Cost / task" sortKey="cost_per_task" activeKey={sortKey} direction={sortDirection} onSort={toggleSort} />
              <SortableHeader label="Success" sortKey="success_rate" activeKey={sortKey} direction={sortDirection} onSort={toggleSort} />
              <SortableHeader label="Avg reopens" sortKey="avg_reopens" activeKey={sortKey} direction={sortDirection} onSort={toggleSort} />
              <SortableHeader label="Tasks" sortKey="task_count" activeKey={sortKey} direction={sortDirection} onSort={toggleSort} />
              <SortableHeader label="Tokens" sortKey="tokens" activeKey={sortKey} direction={sortDirection} onSort={toggleSort} />
            </tr>
          </thead>
          <tbody className="divide-y divide-border">
            {sortedRows.map((row) => {
              const hasDetail = hasRowDetail(row);
              const open = expanded[row.id] ?? false;
              return (
                <tr key={row.id} className="align-top hover:bg-muted/30">
                  <td colSpan={8} className="p-0">
                    <Collapsible open={open} onOpenChange={(next) => setExpanded((current) => ({ ...current, [row.id]: next }))}>
                      <div className="grid grid-cols-[40px_minmax(260px,1.4fr)_repeat(6,minmax(110px,1fr))] items-start">
                        <div className="px-3 py-3">
                          {hasDetail ? (
                            <CollapsibleTrigger
                              className="flex h-6 w-6 items-center justify-center rounded border border-border text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                              aria-label={open ? "Collapse row details" : "Expand row details"}
                            >
                              <span className={cn("text-xs transition-transform", open && "rotate-90")}>›</span>
                            </CollapsibleTrigger>
                          ) : (
                            <span className="block h-6 w-6" />
                          )}
                        </div>
                        <div className="min-w-0 px-4 py-3">
                          <EntityLabel row={row} kind={config.kind} />
                        </div>
                        <MetricText value={formatCurrency(row.cost)} />
                        <MetricText value={formatCurrency(row.cost_per_task)} />
                        <MetricText value={formatPercent(row.success_rate)} />
                        <MetricText value={formatAverageReopens(row.avg_reopens)} />
                        <MetricText value={formatInteger(row.task_count)} />
                        <MetricText value={formatCompactNumber(row.tokens_in + row.tokens_out)} detail={`${formatCompactNumber(row.tokens_in)} in · ${formatCompactNumber(row.tokens_out)} out`} />
                      </div>
                      {hasDetail && (
                        <CollapsibleContent>
                          <RowDetail row={row} />
                        </CollapsibleContent>
                      )}
                    </Collapsible>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function SortableHeader({
  label,
  sortKey,
  activeKey,
  direction,
  onSort,
}: {
  label: string;
  sortKey: SortKey;
  activeKey: SortKey;
  direction: SortDirection;
  onSort: (key: SortKey) => void;
}) {
  const active = sortKey === activeKey;
  return (
    <th className="px-4 py-3 font-medium">
      <Button
        variant="ghost"
        size="sm"
        className="h-auto px-0 py-0 text-xs font-medium text-muted-foreground hover:bg-transparent hover:text-foreground"
        onClick={() => onSort(sortKey)}
      >
        {label}
        {active && (
          <HugeiconsIcon
            icon={direction === "asc" ? ArrowUp01Icon : ArrowDown01Icon}
            size={12}
            className="ml-1"
          />
        )}
      </Button>
    </th>
  );
}

function MetricText({ value, detail }: { value: string; detail?: string }) {
  return (
    <div className="px-4 py-3 tabular-nums text-foreground">
      <div>{value}</div>
      {detail && <div className="text-xs text-muted-foreground">{detail}</div>}
    </div>
  );
}

function EntityLabel({ row, kind }: { row: UsageBreakdownRow; kind: BreakdownKind }) {
  const href = getEntityHref(row, kind);
  const label = row.name || row.id;
  const id = getEntityId(row, kind);

  return (
    <div className="min-w-0">
      {href ? (
        <a
          href={href}
          className="inline-flex max-w-full items-center gap-1 truncate font-medium text-primary hover:underline"
          title={label}
        >
          <span className="truncate">{label}</span>
          <HugeiconsIcon icon={ArrowRight01Icon} size={12} className="shrink-0" />
        </a>
      ) : (
        <div className="truncate font-medium text-foreground" title={label}>{label}</div>
      )}
      {id && id !== label && (
        <div className="truncate text-xs text-muted-foreground" title={id}>{id}</div>
      )}
    </div>
  );
}

function RowDetail({ row }: { row: UsageBreakdownRow }) {
  return (
    <div className="border-t border-border bg-muted/20 px-4 py-4">
      <div className="grid gap-4 lg:grid-cols-2">
        <DetailPanel title="Per-model split" empty="No per-model detail supplied">
          {row.model_split && row.model_split.length > 0 && <ModelSplitTable rows={row.model_split} />}
        </DetailPanel>
        <DetailPanel title="Time series" empty="No time-series detail supplied">
          {row.time_series && row.time_series.length > 0 && <TimeSeriesTable rows={row.time_series} />}
        </DetailPanel>
      </div>
    </div>
  );
}

function DetailPanel({
  title,
  empty,
  children,
}: {
  title: string;
  empty: string;
  children?: ReactNode;
}) {
  return (
    <div className="rounded-md border border-border bg-card/70 p-3">
      <h3 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">{title}</h3>
      {children ?? <p className="mt-3 text-sm text-muted-foreground">{empty}</p>}
    </div>
  );
}

function ModelSplitTable({ rows }: { rows: UsageModelSplit[] }) {
  return (
    <div className="mt-3 max-h-72 overflow-auto">
      <table className="w-full min-w-[560px] text-left text-xs">
        <thead className="text-muted-foreground">
          <tr>
            <th className="py-2 pr-3 font-medium">Model</th>
            <th className="py-2 pr-3 font-medium">Spend</th>
            <th className="py-2 pr-3 font-medium">Cost/task</th>
            <th className="py-2 pr-3 font-medium">Success</th>
            <th className="py-2 pr-3 font-medium">Tasks</th>
            <th className="py-2 pr-3 font-medium">Tokens</th>
          </tr>
        </thead>
        <tbody className="divide-y divide-border">
          {rows.map((row) => (
            <tr key={row.model}>
              <td className="max-w-[180px] py-2 pr-3 font-medium text-foreground"><span className="block truncate" title={row.model}>{row.model}</span></td>
              <td className="py-2 pr-3 tabular-nums text-foreground">{formatCurrency(row.cost)}</td>
              <td className="py-2 pr-3 tabular-nums text-foreground">{formatCurrency(row.cost_per_task)}</td>
              <td className="py-2 pr-3 tabular-nums text-foreground">{formatPercent(row.success_rate)}</td>
              <td className="py-2 pr-3 tabular-nums text-foreground">{formatInteger(row.task_count)}</td>
              <td className="py-2 pr-3 tabular-nums text-foreground">{formatCompactNumber(totalTokens(row.tokens_in, row.tokens_out, row.total_tokens))}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function TimeSeriesTable({ rows }: { rows: UsageTimeSeriesPoint[] }) {
  return (
    <div className="mt-3 max-h-72 overflow-auto">
      <table className="w-full min-w-[440px] text-left text-xs">
        <thead className="text-muted-foreground">
          <tr>
            <th className="py-2 pr-3 font-medium">Bucket</th>
            <th className="py-2 pr-3 font-medium">Spend</th>
            <th className="py-2 pr-3 font-medium">Tasks</th>
            <th className="py-2 pr-3 font-medium">Tokens</th>
            <th className="py-2 pr-3 font-medium">Group</th>
          </tr>
        </thead>
        <tbody className="divide-y divide-border">
          {rows.map((row, index) => (
            <tr key={`${row.date}-${row.group_key ?? index}`}>
              <td className="py-2 pr-3 text-foreground">{formatBucket(row.date)}</td>
              <td className="py-2 pr-3 tabular-nums text-foreground">{formatCurrency(row.cost)}</td>
              <td className="py-2 pr-3 tabular-nums text-foreground">{formatInteger(row.task_count)}</td>
              <td className="py-2 pr-3 tabular-nums text-foreground">{formatCompactNumber(row.tokens_in + row.tokens_out)}</td>
              <td className="max-w-[160px] py-2 pr-3 text-muted-foreground"><span className="block truncate" title={row.group_key ?? undefined}>{row.group_key ?? EM_DASH}</span></td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function sortRows(rows: UsageBreakdownRow[], key: SortKey, direction: SortDirection): UsageBreakdownRow[] {
  const sign = direction === "asc" ? 1 : -1;
  return [...rows].sort((a, b) => compareRows(a, b, key) * sign || a.name.localeCompare(b.name));
}

function compareRows(a: UsageBreakdownRow, b: UsageBreakdownRow, key: SortKey): number {
  if (key === "name") return a.name.localeCompare(b.name);
  if (key === "tokens") return a.tokens_in + a.tokens_out - (b.tokens_in + b.tokens_out);
  return numericSortValue(a[key]) - numericSortValue(b[key]);
}

function numericSortValue(value: number | null | undefined): number {
  return value ?? Number.NEGATIVE_INFINITY;
}

function hasRowDetail(row: UsageBreakdownRow): boolean {
  return Boolean(row.model_split?.length || row.time_series?.length);
}

function getEntityId(row: UsageBreakdownRow, kind: BreakdownKind): string | null | undefined {
  if (kind === "task") return row.task_id ?? row.id;
  if (kind === "proposal") return row.proposal_id ?? row.id;
  return row.id;
}

function getEntityHref(row: UsageBreakdownRow, kind: BreakdownKind): string | null | undefined {
  if (kind === "task") return row.task_url ?? row.url ?? (row.task_id ? `/task/${row.task_id}` : undefined);
  if (kind === "proposal") {
    return row.proposal_url ?? row.url ?? (row.proposal_id ? `/proposals/${row.proposal_id}` : undefined);
  }
  return row.url;
}
