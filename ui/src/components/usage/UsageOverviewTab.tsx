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
import {
  EM_DASH,
  FALLBACK_GROUP,
  formatAgentRole,
  formatBucket,
  formatCompactNumber,
  formatCurrency,
  formatInteger,
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

interface SplitSeriesRow {
  bucket: string;
  group: string;
  actual: number | null;
  projected: number | null;
  unpriced_count: number;
}

interface SplitBarRow {
  id: string;
  label: string;
  actual: number | null;
  projected: number | null;
  tokens: number;
  cached: number;
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

export function UsageOverviewTab({ data }: { data: UsageAnalyticsResponse }) {
  const [grouping, setGrouping] = useState<SpendGrouping>("model");

  const seriesRows = useMemo(
    () => buildSplitSeriesRows(data.time_series ?? [], data, grouping),
    [data, grouping],
  );

  const actualStacked = useMemo(
    () => buildStackedSpend(seriesRows, "actual"),
    [seriesRows],
  );
  const projectedStacked = useMemo(
    () => buildStackedSpend(seriesRows, "projected"),
    [seriesRows],
  );
  const costByModel = useMemo(() => buildSplitBarRows(data, "model"), [data]);
  const costByAgent = useMemo(() => buildSplitBarRows(data, "agent"), [data]);

  return (
    <div className="space-y-4">
      <SplitKpiRow data={data} />

      <SplitChartSection
        title="Cost over time"
        description="Actual API spend and projected subscription-equivalent cost shown separately. Unpriced sessions are excluded from both figures."
        action={
          <div className="flex items-center gap-2">
            <Label className="text-xs text-muted-foreground">Group by</Label>
            <Select
              items={GROUPING_OPTIONS}
              value={grouping}
              onValueChange={(value) => {
                if (isSpendGrouping(value)) setGrouping(value);
              }}
            >
              <SelectTrigger
                className="h-8 w-[140px] text-xs"
                title="Spend grouping"
              >
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
        actualChart={
          actualStacked.hasPricedCost ? (
            <ResponsiveBar
              data={actualStacked.data}
              keys={actualStacked.keys}
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
              legends={buildLegend(actualStacked.keys)}
            />
          ) : (
            <ChartEmptyState
              title={
                data.time_series.length > 0
                  ? "No actual API spend in this range"
                  : "No time-series points"
              }
              description="Actual API spend is unavailable for the selected filters."
              details={buildTokenSummary(data.time_series)}
            />
          )
        }
        projectedChart={
          projectedStacked.hasPricedCost ? (
            <ResponsiveBar
              data={projectedStacked.data}
              keys={projectedStacked.keys}
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
              legends={buildLegend(projectedStacked.keys)}
            />
          ) : (
            <ChartEmptyState
              title={
                data.time_series.length > 0
                  ? "No projected cost in this range"
                  : "No time-series points"
              }
              description="Projected subscription-equivalent cost is unavailable for the selected filters."
              details={buildTokenSummary(data.time_series)}
            />
          )
        }
      />

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
        <SplitBarChart title="Cost by model" rows={costByModel} />
        <SplitBarChart title="Cost by agent role" rows={costByAgent} />
      </div>

      <MethodologyDisclosure />
    </div>
  );
}

// ── Split KPI row ────────────────────────────────────────────────────────────

function SplitKpiRow({ data }: { data: UsageAnalyticsResponse }) {
  const kpis = data.kpis ?? [];
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

  // Aggregate split fields across all KPIs.
  const totalActual = kpis.reduce(
    (sum, k) => sum + (k.actual_spend_usd ?? 0),
    0,
  );
  const totalProjected = kpis.reduce(
    (sum, k) => sum + (k.projected_usd ?? 0),
    0,
  );
  const totalUnpriced = kpis.reduce(
    (sum, k) => sum + (k.unpriced_count ?? 0),
    0,
  );

  // Also include top-level unpriced_session_count when present.
  const unpricedTotal = totalUnpriced || data.unpriced_session_count || 0;

  // Check if split fields are present at all (backward compat with pre-split).
  const hasSplitFields = kpis.some(
    (k) => k.actual_spend_usd !== undefined || k.projected_usd !== undefined,
  );

  // Token total from existing KPIs (find the tokens KPI if present).
  const tokensKpi = kpis.find((k) => k.label.toLowerCase().includes("token"));

  return (
    <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-4">
      {/* Actual API spend */}
      <SimpleKpiCard
        label="Actual API spend"
        value={hasSplitFields ? formatCurrency(totalActual) : EM_DASH}
        caption="Real API-key spend from billed sessions"
        testId="usage-split-kpi-actual-spend"
      />
      {/* Projected subscription-equivalent cost */}
      <SimpleKpiCard
        label="Projected subscription-equivalent cost"
        value={hasSplitFields ? formatCurrency(totalProjected) : EM_DASH}
        caption="List-price equivalent for flat-rate plan usage"
        testId="usage-split-kpi-projected-cost"
      />
      {/* Unpriced sessions */}
      <SimpleKpiCard
        label="Unpriced sessions"
        value={unpricedTotal > 0 ? formatInteger(unpricedTotal) : "0"}
        caption="Excluded from both cost figures"
        testId="usage-split-kpi-unpriced-sessions"
      />
      {/* Tokens (re-use existing tokens KPI or compute) */}
      {tokensKpi ? (
        <BreakdownKpiCard kpi={tokensKpi} />
      ) : (
        <SimpleKpiCard label="Tokens" value={EM_DASH} caption="No token data" />
      )}
    </div>
  );
}

function SimpleKpiCard({
  label,
  value,
  caption,
  testId,
}: {
  label: string;
  value: string;
  caption?: string;
  testId?: string;
}) {
  return (
    <div
      className="flex h-full min-w-0 flex-col rounded-lg border border-border bg-card p-4"
      data-testid={testId}
    >
      <p className="truncate text-xs text-muted-foreground">{label}</p>
      <p className="mt-1 truncate text-2xl font-semibold tabular-nums tracking-tight text-foreground">
        {value}
      </p>
      {caption && (
        <p
          className="mt-auto truncate pt-1 text-[11px] text-muted-foreground"
          title={caption}
        >
          {caption}
        </p>
      )}
    </div>
  );
}

function BreakdownKpiCard({ kpi }: { kpi: UsageKpi }) {
  return (
    <div className="flex h-full min-w-0 flex-col rounded-lg border border-border bg-card p-4">
      <p className="truncate text-xs text-muted-foreground">{kpi.label}</p>
      <p className="mt-1 truncate text-2xl font-semibold tabular-nums tracking-tight text-foreground">
        {formatKpiValue(kpi)}
      </p>
      {kpi.caption && (
        <p
          className="mt-1 truncate text-[11px] text-muted-foreground"
          title={kpi.caption}
        >
          {kpi.caption}
        </p>
      )}
      {kpi.breakdown && kpi.breakdown.length > 0 && (
        <div className="mt-auto flex flex-wrap gap-x-3 gap-y-1 pt-2">
          {kpi.breakdown.map((part) => (
            <span
              key={part.label}
              className="text-[11px] text-muted-foreground"
              title={`${part.label}: ${part.value.toLocaleString()}`}
            >
              {part.label}{" "}
              <span className="tabular-nums font-medium text-foreground">
                {formatCompactNumber(part.value)}
              </span>
            </span>
          ))}
        </div>
      )}
    </div>
  );
}

// ── Split chart section ──────────────────────────────────────────────────────

function SplitChartSection({
  title,
  description,
  action,
  actualChart,
  projectedChart,
}: {
  title: string;
  description?: string;
  action?: ReactNode;
  actualChart: ReactNode;
  projectedChart: ReactNode;
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
      <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
        <div>
          <p className="mb-2 text-xs font-medium text-muted-foreground">
            Actual API spend
          </p>
          <div className="h-[280px]">{actualChart}</div>
        </div>
        <div>
          <p className="mb-2 text-xs font-medium text-muted-foreground">
            Projected subscription-equivalent cost
          </p>
          <div className="h-[280px]">{projectedChart}</div>
        </div>
      </div>
    </section>
  );
}

// ── Split bar chart ──────────────────────────────────────────────────────────

function SplitBarChart({
  title,
  rows,
}: {
  title: string;
  rows: SplitBarRow[];
}) {
  const actualChartData = useMemo(
    () => buildBarChartData(rows, "actual"),
    [rows],
  );
  const projectedChartData = useMemo(
    () => buildBarChartData(rows, "projected"),
    [rows],
  );

  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-3 text-sm font-medium text-foreground">{title}</h3>
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <div>
          <p className="mb-2 text-xs font-medium text-muted-foreground">
            Actual
          </p>
          {actualChartData.hasPricedCost ? (
            <div className="h-[260px]">
              <ResponsiveBar
                data={actualChartData.data}
                keys={["Actual"]}
                indexBy="label"
                margin={{ top: 10, right: 18, bottom: 72, left: 72 }}
                padding={0.35}
                colors={["#60a5fa"]}
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
                tooltip={({ data, color }) => (
                  <SplitBarTooltip data={data} color={color} kind="Actual" />
                )}
              />
            </div>
          ) : (
            <ChartEmptyState
              title={rows.length > 0 ? "No actual spend" : "No rows"}
              description="Actual API spend is unavailable for these rows."
              rows={rows.map((row) => ({
                label: row.label,
                value: EM_DASH,
                detail: `${formatCompactNumber(row.tokens)} tokens`,
              }))}
            />
          )}
        </div>
        <div>
          <p className="mb-2 text-xs font-medium text-muted-foreground">
            Projected subscription-equivalent
          </p>
          {projectedChartData.hasPricedCost ? (
            <div className="h-[260px]">
              <ResponsiveBar
                data={projectedChartData.data}
                keys={["Projected"]}
                indexBy="label"
                margin={{ top: 10, right: 18, bottom: 72, left: 72 }}
                padding={0.35}
                colors={["#34d399"]}
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
                tooltip={({ data, color }) => (
                  <SplitBarTooltip data={data} color={color} kind="Projected" />
                )}
              />
            </div>
          ) : (
            <ChartEmptyState
              title={rows.length > 0 ? "No projected cost" : "No rows"}
              description="Projected subscription-equivalent cost is unavailable for these rows."
              rows={rows.map((row) => ({
                label: row.label,
                value: EM_DASH,
                detail: `${formatCompactNumber(row.tokens)} tokens`,
              }))}
            />
          )}
        </div>
      </div>
    </section>
  );
}

function SplitBarTooltip({
  data,
  color,
  kind,
}: {
  data: ChartDatum & {
    fullLabel?: string;
    Actual?: number;
    Projected?: number;
    Tokens?: number;
    Cached?: number;
  };
  color: string;
  kind: string;
}) {
  const tokens = Number(data.Tokens ?? 0);
  const cached = Number(data.Cached ?? 0);
  const value =
    kind === "Actual" ? Number(data.Actual ?? 0) : Number(data.Projected ?? 0);
  return (
    <div className="rounded-md border border-border bg-popover px-2.5 py-1.5 text-xs shadow-md">
      <div className="flex items-center gap-1.5 font-medium text-foreground">
        <span
          className="inline-block h-2 w-2 rounded-[2px]"
          style={{ backgroundColor: color }}
          aria-hidden
        />
        <span className="truncate" title={data.fullLabel ?? String(data.label)}>
          {data.fullLabel ?? String(data.label)}
        </span>
      </div>
      <div className="mt-1 tabular-nums text-foreground">
        {formatCurrency(value)}
      </div>
      <div className="mt-0.5 tabular-nums text-muted-foreground">
        {formatCompactNumber(tokens)} tokens · {formatCompactNumber(cached)}{" "}
        cached
      </div>
    </div>
  );
}

// ── Empty state ──────────────────────────────────────────────────────────────

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
                <span className="truncate text-muted-foreground">
                  {row.label}
                </span>
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

// ── Methodology disclosure ───────────────────────────────────────────────────

function MethodologyDisclosure() {
  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="text-sm font-medium text-foreground">
        How costs are calculated
      </h3>
      <div className="mt-2 space-y-2 text-xs text-muted-foreground">
        <p>
          <span className="font-medium text-foreground">Actual API spend</span>{" "}
          is real billed API-key usage. Sessions run through API keys contribute
          their list-rate cost to this figure.
        </p>
        <p>
          <span className="font-medium text-foreground">
            Projected subscription-equivalent cost
          </span>{" "}
          is a list-price equivalent estimate for sessions run through flat-rate
          subscription or coding-plan access. These sessions incur $0 in real
          API charges — the projected figure shows what the usage <em>would</em>{" "}
          cost at catalog list rates, and is not a spend figure.
        </p>
        <p>
          <span className="font-medium text-foreground">Unpriced sessions</span>{" "}
          are excluded from both figures because they had no matching catalog
          pricing. They are counted separately so you can see how much usage is
          unaccounted for.
        </p>
      </div>
    </section>
  );
}

// ── Helpers ──────────────────────────────────────────────────────────────────

function isSpendGrouping(value: string | null): value is SpendGrouping {
  return GROUPING_OPTIONS.some((option) => option.value === value);
}

function buildSplitSeriesRows(
  points: UsageTimeSeriesPoint[],
  data: UsageAnalyticsResponse,
  grouping: SpendGrouping,
): SplitSeriesRow[] {
  const projectNames = new Map<string, string>();
  for (const row of data.breakdowns?.by_project ?? [])
    projectNames.set(row.id, row.name);
  for (const cell of data.project_model_matrix ?? []) {
    projectNames.set(cell.project_id, cell.project_name);
  }

  return points.map((point) => ({
    bucket: formatBucket(point.date),
    group: getPointGroup(point, grouping, projectNames),
    actual: point.actual_spend_usd ?? null,
    projected: point.projected_usd ?? null,
    unpriced_count: point.unpriced_count ?? 0,
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
    const projectName =
      point.project_name ?? projectNames.get(point.project_id ?? "");
    return projectName ?? point.project_id ?? point.group_key ?? FALLBACK_GROUP;
  }

  return formatAgentRole(
    point.agent_type ?? point.agent_role ?? point.group_key,
  );
}

function buildStackedSpend(
  rows: SplitSeriesRow[],
  basis: "actual" | "projected",
): {
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
    const value = basis === "actual" ? row.actual : row.projected;
    if (value !== null) {
      hasPricedCost = true;
      bucket[row.group] = Number(bucket[row.group] ?? 0) + value;
    }
  }

  return {
    data: Array.from(byBucket.values()),
    keys,
    hasPricedCost,
  };
}

function buildSplitBarRows(
  data: UsageAnalyticsResponse,
  dimension: "model" | "agent",
): SplitBarRow[] {
  if (dimension === "model") {
    const modelRows = data.model_effectiveness
      .filter((row) => row.total_tokens > 0)
      .map((row) => ({
        id: row.model,
        label: row.model,
        actual: row.actual_spend_usd ?? null,
        projected: row.projected_usd ?? null,
        tokens: row.total_tokens,
        cached: row.tokens_cached ?? 0,
      }));
    if (modelRows.length > 0) return sortAndLimitSplitRows(modelRows);
  }

  // Fallback: rollup from time series.
  const groupFn =
    dimension === "model"
      ? (point: UsageTimeSeriesPoint) => point.model
      : (point: UsageTimeSeriesPoint) =>
          point.agent_type || point.agent_role
            ? formatAgentRole(point.agent_type ?? point.agent_role)
            : undefined;

  return sortAndLimitSplitRows(
    rollupSplitTimeSeries(data.time_series, groupFn),
  );
}

function rollupSplitTimeSeries(
  points: UsageTimeSeriesPoint[],
  groupFn: (point: UsageTimeSeriesPoint) => string | undefined,
): SplitBarRow[] {
  const byGroup = new Map<string, SplitBarRow>();
  for (const point of points) {
    const label = groupFn(point) ?? point.group_key ?? FALLBACK_GROUP;
    const current = byGroup.get(label) ?? {
      id: label,
      label,
      actual: null,
      projected: null,
      tokens: 0,
      cached: 0,
    };
    if (point.actual_spend_usd != null)
      current.actual = (current.actual ?? 0) + point.actual_spend_usd;
    if (point.projected_usd != null)
      current.projected = (current.projected ?? 0) + point.projected_usd;
    // Fall back to legacy cost field for pre-split responses.
    if (
      current.actual === null &&
      current.projected === null &&
      point.cost != null
    ) {
      current.actual = (current.actual ?? 0) + point.cost;
    }
    current.tokens += point.tokens_in + point.tokens_out;
    current.cached += point.tokens_cached ?? 0;
    byGroup.set(label, current);
  }
  return Array.from(byGroup.values());
}

function sortAndLimitSplitRows(rows: SplitBarRow[]): SplitBarRow[] {
  return [...rows]
    .sort(
      (a, b) =>
        (b.actual ?? 0) +
          (b.projected ?? 0) -
          ((a.actual ?? 0) + (a.projected ?? 0)) || b.tokens - a.tokens,
    )
    .slice(0, 10);
}

function buildBarChartData(
  rows: SplitBarRow[],
  basis: "actual" | "projected",
): {
  data: ChartDatum[];
  hasPricedCost: boolean;
} {
  let hasPricedCost = false;
  const key = basis === "actual" ? "Actual" : "Projected";
  const data = rows.map((row) => {
    const value = basis === "actual" ? row.actual : row.projected;
    if (value !== null) hasPricedCost = true;
    return {
      bucket: row.id,
      label: truncateLabel(row.label),
      fullLabel: row.label,
      [key]: value ?? 0,
      Tokens: row.tokens,
      Cached: row.cached,
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

function formatKpiValue(kpi: UsageKpi): string {
  if (kpi.value === null) return EM_DASH;
  return kpi.formatted || formatCompactNumber(kpi.value);
}

function buildTokenSummary(points: UsageTimeSeriesPoint[]): string | undefined {
  const tokens = points.reduce(
    (sum, point) => sum + point.tokens_in + point.tokens_out,
    0,
  );
  return tokens > 0
    ? `${formatCompactNumber(tokens)} tokens in range`
    : undefined;
}
