import { type ReactNode, useMemo, useState } from "react";
import { ResponsiveBar } from "@nivo/bar";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Label } from "@/components/ui/label";
import type {
  UsageAnalyticsResponse,
  UsageKpi,
  UsageTimeSeriesPoint,
} from "@/api/analytics";
import { cn } from "@/lib/utils";
import {
  EM_DASH,
  FALLBACK_GROUP,
  formatAgentRole,
  formatBucket,
  formatCompactNumber,
  formatCurrency,
  formatDeltaPercent,
  truncateLabel,
} from "./usageFormatters";

const GROUPING_OPTIONS = [
  { value: "model", label: "Model" },
  { value: "project", label: "Project" },
  { value: "agent", label: "Agent role" },
] as const;

type SpendGrouping = (typeof GROUPING_OPTIONS)[number]["value"];

type ChartDatum = Record<string, string | number> & {
  bucket: string;
};

interface SeriesRow {
  bucket: string;
  group: string;
  cost: number | null;
}

interface BarRow {
  id: string;
  label: string;
  cost: number | null;
  tokens: number;
}

const CHART_COLORS = [
  "#60a5fa",
  "#34d399",
  "#fbbf24",
  "#c084fc",
  "#f87171",
  "#22d3ee",
  "#fb7185",
  "#a78bfa",
  "#4ade80",
  "#f97316",
];

const nivoTheme = {
  text: { fill: "oklch(0.705 0.015 286.067)" },
  axis: {
    ticks: { text: { fill: "oklch(0.705 0.015 286.067)", fontSize: 11 } },
    legend: { text: { fill: "oklch(0.705 0.015 286.067)", fontSize: 12 } },
  },
  grid: { line: { stroke: "oklch(1 0 0 / 6%)" } },
  tooltip: {
    container: {
      background: "oklch(0.21 0.006 285.885)",
      color: "oklch(0.985 0 0)",
      borderRadius: "8px",
      border: "1px solid oklch(1 0 0 / 10%)",
      fontSize: "12px",
      boxShadow: "0 4px 12px oklch(0 0 0 / 30%)",
    },
  },
};

export function UsageOverviewTab({
  data,
}: {
  data: UsageAnalyticsResponse;
}) {
  const [grouping, setGrouping] = useState<SpendGrouping>("model");

  const seriesRows = useMemo(
    () => buildSeriesRows(data.time_series ?? [], data, grouping),
    [data, grouping],
  );

  const stackedSpend = useMemo(() => buildStackedSpend(seriesRows), [seriesRows]);
  const spendByModel = useMemo(() => buildSpendByModel(data), [data]);
  const spendByAgent = useMemo(() => buildSpendByAgentRole(data), [data]);

  return (
    <div className="space-y-4">
      <KpiRow kpis={data.kpis ?? []} />

      <ChartSection
        title="Spend over time"
        description="Stacked priced spend for the selected grouping. Unpriced/null cost buckets are intentionally excluded from the spend stack."
        action={
          <div className="flex items-center gap-2">
            <Label className="text-xs text-muted-foreground">Group by</Label>
            <Select
              value={grouping}
              onValueChange={(value) => {
                if (isSpendGrouping(value)) setGrouping(value);
              }}
            >
              <SelectTrigger className="h-8 w-[140px] text-xs" title="Spend grouping">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {GROUPING_OPTIONS.map((option) => (
                  <SelectItem key={option.value} value={option.value}>
                    {option.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        }
      >
        {stackedSpend.hasPricedCost ? (
          <ResponsiveBar
            data={stackedSpend.data}
            keys={stackedSpend.keys}
            indexBy="bucket"
            margin={{ top: 10, right: 24, bottom: 54, left: 72 }}
            padding={0.25}
            innerPadding={1}
            groupMode="stacked"
            colors={CHART_COLORS}
            borderRadius={2}
            valueFormat={(value) => formatCurrency(Number(value))}
            axisBottom={{ tickSize: 0, tickPadding: 8, tickRotation: -30 }}
            axisLeft={{
              tickSize: 0,
              tickPadding: 8,
              format: (value) => formatCurrency(Number(value)),
            }}
            enableLabel={false}
            theme={nivoTheme}
            legends={buildLegend(stackedSpend.keys)}
          />
        ) : (
          <ChartEmptyState
            title={
              data.time_series.length > 0
                ? "No priced cost in this range"
                : "No time-series points"
            }
            description="Cost is unavailable for the selected filters, so spend renders as — instead of $0. Token totals remain visible in KPI cards and tables where provided."
            details={buildTokenSummary(data.time_series)}
          />
        )}
      </ChartSection>

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
        <SpendBarChart title="Spend by model" rows={spendByModel} />
        <SpendBarChart title="Spend by agent role" rows={spendByAgent} />
      </div>
    </div>
  );
}

function KpiRow({ kpis }: { kpis: UsageKpi[] }) {
  if (kpis.length === 0) {
    return (
      <div className="rounded-lg border border-dashed border-border bg-card/50 px-4 py-8 text-center">
        <p className="text-sm font-medium text-foreground">No KPI data</p>
        <p className="mt-1 text-sm text-muted-foreground">
          KPI totals will appear once usage exists for the selected filters.
        </p>
      </div>
    );
  }

  return (
    <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-4">
      {kpis.map((kpi) => (
        <KpiCard key={kpi.label} kpi={kpi} />
      ))}
    </div>
  );
}

function KpiCard({ kpi }: { kpi: UsageKpi }) {
  const deltaState = getDeltaState(kpi.delta_pct);
  const value = formatKpiValue(kpi);

  return (
    <div className="min-w-0 rounded-lg border border-border bg-card p-4">
      <p className="truncate text-xs text-muted-foreground">{kpi.label}</p>
      <p className="mt-1 truncate text-2xl font-semibold tabular-nums tracking-tight text-foreground">
        {value}
      </p>
      <div
        className={cn(
          "mt-2 inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium",
          deltaState.className,
        )}
        title="Period-over-period delta"
      >
        <span className="mr-1" aria-hidden>
          {deltaState.symbol}
        </span>
        {formatDelta(kpi.delta_pct)}
      </div>
    </div>
  );
}

function ChartSection({
  title,
  description,
  action,
  children,
}: {
  title: string;
  description?: string;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <div className="mb-3 flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 className="text-sm font-medium text-foreground">{title}</h3>
          {description && (
            <p className="mt-1 max-w-3xl text-xs text-muted-foreground">
              {description}
            </p>
          )}
        </div>
        {action}
      </div>
      <div className="h-[320px]">{children}</div>
    </section>
  );
}

function SpendBarChart({ title, rows }: { title: string; rows: BarRow[] }) {
  const chart = useMemo(() => buildBarChart(rows), [rows]);

  return (
    <ChartSection title={title}>
      {chart.hasPricedCost ? (
        <ResponsiveBar
          data={chart.data}
          keys={["Spend"]}
          indexBy="label"
          margin={{ top: 10, right: 18, bottom: 72, left: 72 }}
          padding={0.35}
          colors={[CHART_COLORS[0]]}
          borderRadius={4}
          valueFormat={(value) => formatCurrency(Number(value))}
          axisBottom={{ tickSize: 0, tickPadding: 8, tickRotation: -35 }}
          axisLeft={{
            tickSize: 0,
            tickPadding: 8,
            format: (value) => formatCurrency(Number(value)),
          }}
          enableLabel={false}
          theme={nivoTheme}
        />
      ) : (
        <ChartEmptyState
          title={rows.length > 0 ? "No priced cost" : "No rows"}
          description="Spend is unavailable for these rows and is shown as — rather than $0."
          rows={rows.map((row) => ({
            label: row.label,
            value: EM_DASH,
            detail: `${formatCompactNumber(row.tokens)} tokens`,
          }))}
        />
      )}
    </ChartSection>
  );
}

function ChartEmptyState({
  title,
  description,
  details,
  rows,
}: {
  title: string;
  description: string;
  details?: string;
  rows?: { label: string; value: string; detail: string }[];
}) {
  return (
    <div className="flex h-full items-center justify-center rounded-md border border-dashed border-border bg-muted/20 p-6 text-center">
      <div>
        <p className="text-sm font-medium text-foreground">{title}</p>
        <p className="mt-1 max-w-md text-sm text-muted-foreground">
          {description}
        </p>
        {details && (
          <p className="mt-2 text-xs font-medium text-muted-foreground">
            {details}
          </p>
        )}
        {rows && rows.length > 0 && (
          <div className="mt-3 max-h-28 space-y-1 overflow-y-auto text-left">
            {rows.slice(0, 6).map((row) => (
              <div
                key={row.label}
                className="flex items-center justify-between gap-3 rounded bg-background/40 px-2 py-1 text-xs"
              >
                <span className="truncate text-muted-foreground">{row.label}</span>
                <span className="shrink-0 tabular-nums text-foreground">
                  {row.value} · {row.detail}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

function isSpendGrouping(value: string | null): value is SpendGrouping {
  return GROUPING_OPTIONS.some((option) => option.value === value);
}

function buildSeriesRows(
  points: UsageTimeSeriesPoint[],
  data: UsageAnalyticsResponse,
  grouping: SpendGrouping,
): SeriesRow[] {
  const projectNames = new Map<string, string>();
  for (const row of data.breakdowns?.by_project ?? []) projectNames.set(row.id, row.name);
  for (const cell of data.project_model_matrix ?? []) {
    projectNames.set(cell.project_id, cell.project_name);
  }

  return points.map((point) => ({
    bucket: formatBucket(point.date),
    group: getPointGroup(point, grouping, projectNames),
    cost: point.cost,
  }));
}

function getPointGroup(
  point: UsageTimeSeriesPoint,
  grouping: SpendGrouping,
  projectNames: Map<string, string>,
): string {
  if (grouping === "model") {
    return point.model ?? point.group_key ?? FALLBACK_GROUP;
  }

  if (grouping === "project") {
    const projectName = point.project_name ?? projectNames.get(point.project_id ?? "");
    return projectName ?? point.project_id ?? point.group_key ?? FALLBACK_GROUP;
  }

  return formatAgentRole(point.agent_type ?? point.agent_role ?? point.group_key);
}

function buildStackedSpend(rows: SeriesRow[]): {
  data: ChartDatum[];
  keys: string[];
  hasPricedCost: boolean;
} {
  const keys = Array.from(new Set(rows.map((row) => row.group))).sort();
  const byBucket = new Map<string, ChartDatum>();
  let hasPricedCost = false;

  for (const row of rows) {
    let bucket = byBucket.get(row.bucket);
    if (!bucket) {
      bucket = { bucket: row.bucket };
      byBucket.set(row.bucket, bucket);
    }
    if (row.cost !== null) {
      hasPricedCost = true;
      bucket[row.group] = Number(bucket[row.group] ?? 0) + row.cost;
    }
  }

  return {
    data: Array.from(byBucket.values()),
    keys,
    hasPricedCost,
  };
}

function buildSpendByModel(data: UsageAnalyticsResponse): BarRow[] {
  const modelRows = data.model_effectiveness.map((row) => ({
    id: row.model,
    label: row.model,
    cost: row.total_cost,
    tokens: row.total_tokens,
  }));

  if (modelRows.length > 0) return sortAndLimitRows(modelRows);

  const rolledUp = rollupTimeSeries(data.time_series, (point) => point.model);
  return sortAndLimitRows(rolledUp);
}

function buildSpendByAgentRole(data: UsageAnalyticsResponse): BarRow[] {
  const fromTimeSeries = rollupTimeSeries(data.time_series, (point) =>
    point.agent_type || point.agent_role
      ? formatAgentRole(point.agent_type ?? point.agent_role)
      : undefined,
  );

  return sortAndLimitRows(fromTimeSeries);
}

function rollupTimeSeries(
  points: UsageTimeSeriesPoint[],
  groupFn: (point: UsageTimeSeriesPoint) => string | undefined,
): BarRow[] {
  const byGroup = new Map<string, BarRow>();
  for (const point of points) {
    const label = groupFn(point) ?? point.group_key ?? FALLBACK_GROUP;
    const current = byGroup.get(label) ?? {
      id: label,
      label,
      cost: null,
      tokens: 0,
    };
    if (point.cost !== null) current.cost = (current.cost ?? 0) + point.cost;
    current.tokens += point.tokens_in + point.tokens_out;
    byGroup.set(label, current);
  }
  return Array.from(byGroup.values());
}

function sortAndLimitRows(rows: BarRow[]): BarRow[] {
  return [...rows]
    .sort((a, b) => (b.cost ?? -1) - (a.cost ?? -1) || b.tokens - a.tokens)
    .slice(0, 10);
}

function buildBarChart(rows: BarRow[]): {
  data: ChartDatum[];
  hasPricedCost: boolean;
} {
  let hasPricedCost = false;
  const data = rows.map((row) => {
    if (row.cost !== null) hasPricedCost = true;
    return {
      bucket: row.id,
      label: truncateLabel(row.label),
      Spend: row.cost ?? 0,
      Tokens: row.tokens,
    };
  });
  return { data, hasPricedCost };
}

function buildLegend(keys: string[]) {
  if (keys.length === 0) return [];
  return [
    {
      dataFrom: "keys" as const,
      anchor: "top-right" as const,
      direction: "column" as const,
      translateX: 8,
      translateY: -8,
      itemWidth: 120,
      itemHeight: 18,
      itemTextColor: "oklch(0.705 0.015 286.067)",
      symbolSize: 10,
      symbolShape: "circle" as const,
      effects: [
        {
          on: "hover" as const,
          style: {
            itemTextColor: "oklch(0.985 0 0)",
          },
        },
      ],
    },
  ];
}

function getDeltaState(delta: number | null): {
  symbol: string;
  className: string;
} {
  if (delta === null || Math.abs(delta) < 0.0001) {
    return {
      symbol: "–",
      className: "bg-muted text-muted-foreground",
    };
  }
  if (delta > 0) {
    return {
      symbol: "↗",
      className: "bg-green-500/10 text-green-400",
    };
  }
  return {
    symbol: "↘",
    className: "bg-red-500/10 text-red-400",
  };
}

function formatKpiValue(kpi: UsageKpi): string {
  if (kpi.value === null) return EM_DASH;
  return kpi.formatted || formatCompactNumber(kpi.value);
}

function formatDelta(delta: number | null): string {
  const formatted = formatDeltaPercent(delta);
  return formatted === EM_DASH ? formatted : `${formatted} vs prior`;
}

function buildTokenSummary(points: UsageTimeSeriesPoint[]): string | undefined {
  const tokens = points.reduce(
    (sum, point) => sum + point.tokens_in + point.tokens_out,
    0,
  );
  return tokens > 0 ? `${formatCompactNumber(tokens)} tokens in range` : undefined;
}
