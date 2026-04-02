import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { InlineError } from "@/components/InlineError";
import { cn } from "@/lib/utils";
import { type BaseRole, type AgentMetrics, fetchAgentMetrics } from "@/api/agents";
import { AnalyticsUpIcon, AnalyticsDownIcon, MinusSignIcon, Refresh01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { ResponsiveBar } from "@nivo/bar";
import { ResponsiveLine } from "@nivo/line";
import { ResponsiveRadar } from "@nivo/radar";

const BASE_ROLE_LABELS: Record<BaseRole, string> = {
  worker: "Worker",
  reviewer: "Reviewer",
  lead: "Lead",
  planner: "Planner",
};

const BASE_ROLES: BaseRole[] = ["worker", "reviewer", "lead", "planner"];

const POLL_INTERVAL_MS = 30_000;

// ── Chart colors (matching agentIdentity.ts Tailwind classes) ────────────────

/** Matches text-blue-400 */
const BLUE_400 = "#60a5fa";
/** Matches text-amber-400 */
const AMBER_400 = "#fbbf24";
/** Matches text-red-400 */
const RED_400 = "#f87171";
/** Matches text-purple-400 */
const PURPLE_400 = "#c084fc";

const ROLE_COLORS: Record<BaseRole, string> = {
  worker: BLUE_400,
  reviewer: AMBER_400,
  lead: RED_400,
  planner: PURPLE_400,
};

const TOKEN_BAR_COLORS = [BLUE_400, "#34d399"] as const; // blue for in, emerald for out

// ── Nivo dark theme ──────────────────────────────────────────────────────────

const nivoTheme = {
  text: { fill: "oklch(0.705 0.015 286.067)" },
  axis: {
    ticks: { text: { fill: "oklch(0.705 0.015 286.067)", fontSize: 11 } },
    legend: { text: { fill: "oklch(0.705 0.015 286.067)", fontSize: 12 } },
  },
  grid: { line: { stroke: "oklch(1 0 0 / 6%)" } },
  crosshair: { line: { stroke: "oklch(0.585 0.233 292.697)", strokeWidth: 1 } },
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

// ── Formatters ───────────────────────────────────────────────────────────────

function fmtPct(value: number | null): string {
  if (value === null) return "—";
  return `${Math.round(value * 100)}%`;
}

function fmtAvg(value: number | null): string {
  if (value === null) return "—";
  return value.toFixed(1);
}

function fmtTokens(value: number | null): string {
  if (value === null) return "—";
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (value >= 1_000) return `${Math.round(value / 1_000)}K`;
  return String(Math.round(value));
}

function fmtDuration(seconds: number | null): string {
  if (seconds === null) return "—";
  if (seconds < 60) return `${Math.round(seconds)}s`;
  const m = Math.floor(seconds / 60);
  const s = Math.round(seconds % 60);
  return s > 0 ? `${m}m ${s}s` : `${m}m`;
}

// ── Trend indicator ──────────────────────────────────────────────────────────

function TrendIcon({ trend }: { trend: number | null }) {
  if (trend === null) return <HugeiconsIcon icon={MinusSignIcon} size={14} className="h-3.5 w-3.5 text-muted-foreground" />;
  if (trend > 0.01) return <HugeiconsIcon icon={AnalyticsUpIcon} size={14} className="text-green-500" />;
  if (trend < -0.01) return <HugeiconsIcon icon={AnalyticsDownIcon} size={14} className="text-red-500" />;
  return <HugeiconsIcon icon={MinusSignIcon} size={14} className="h-3.5 w-3.5 text-muted-foreground" />;
}

// ── KPI Card ─────────────────────────────────────────────────────────────────

function KpiCard({
  label,
  value,
  subtext,
  trend,
  hint,
}: {
  label: string;
  value: string;
  subtext?: string;
  trend?: number | null;
  hint?: string;
}) {
  return (
    <div className="rounded-lg border border-border bg-card p-4 space-y-1 flex-1 min-w-0" title={hint}>
      <p className="text-xs text-muted-foreground truncate">{label}</p>
      <div className="flex items-center gap-2">
        <p className="text-2xl font-semibold tabular-nums tracking-tight">{value}</p>
        {trend !== undefined && <TrendIcon trend={trend} />}
      </div>
      {subtext && <p className="text-xs text-muted-foreground">{subtext}</p>}
    </div>
  );
}

// ── Section wrapper ──────────────────────────────────────────────────────────

function ChartSection({
  title,
  height = 280,
  children,
}: {
  title: string;
  height?: number;
  children: React.ReactNode;
}) {
  return (
    <div className="rounded-lg border border-border bg-card p-4 space-y-3">
      <h3 className="text-sm font-medium text-muted-foreground">{title}</h3>
      <div style={{ height }}>{children}</div>
    </div>
  );
}

// ── Main component ───────────────────────────────────────────────────────────

export function AgentMetricsDashboard({ projectId }: { projectId: string | null }) {
  const [metrics, setMetrics] = useState<AgentMetrics[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [lastRefresh, setLastRefresh] = useState<Date | null>(null);
  const pollRef = useRef<number | null>(null);

  const load = useCallback(async (silent = false) => {
    if (!projectId) return;
    if (!silent) setLoading(true);
    setError(null);
    try {
      const data = await fetchAgentMetrics(projectId);
      setMetrics(data.metrics);
      setLastRefresh(new Date());
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load metrics");
    } finally {
      if (!silent) setLoading(false);
    }
  }, [projectId]);

  useEffect(() => {
    void load();
    pollRef.current = window.setInterval(() => void load(true), POLL_INTERVAL_MS);
    return () => {
      if (pollRef.current !== null) window.clearInterval(pollRef.current);
    };
  }, [load]);

  // Aggregate per-role (use defaults only for summary)
  const defaults = useMemo(
    () => (metrics ?? []).filter((m) => m.is_default && m.task_count > 0),
    [metrics],
  );

  // ── KPI aggregates ──────────────────────────────────────────────────────────
  const kpis = useMemo(() => {
    if (!defaults.length) return null;
    const totalTasks = defaults.reduce((s, m) => s + m.task_count, 0);
    const weightedAvg = (fn: (m: AgentMetrics) => number | null) => {
      let sum = 0, weight = 0;
      for (const m of defaults) {
        const v = fn(m);
        if (v !== null) { sum += v * m.task_count; weight += m.task_count; }
      }
      return weight > 0 ? sum / weight : null;
    };
    return {
      totalTasks,
      successRate: weightedAvg((m) => m.success_rate),
      avgTime: weightedAvg((m) => m.avg_time_to_complete_seconds),
      verificationRate: weightedAvg((m) => m.verification_pass_rate),
      successTrend: weightedAvg((m) => m.success_rate_trend),
    };
  }, [defaults]);

  // Deduplicate defaults to one agent per base_role (pick the one with most tasks)
  const perRole = useMemo(() => {
    const best = new Map<BaseRole, AgentMetrics>();
    for (const m of defaults) {
      const prev = best.get(m.base_role);
      if (!prev || m.task_count > prev.task_count) best.set(m.base_role, m);
    }
    // Return in canonical order
    return BASE_ROLES.filter((r) => best.has(r)).map((r) => best.get(r)!);
  }, [defaults]);

  // ── Radar data: per-role comparison ─────────────────────────────────────────
  const radarData = useMemo(() => {
    if (!perRole.length) return [];
    const metricKeys = [
      { key: "Success rate", fn: (m: AgentMetrics) => (m.success_rate ?? 0) * 100 },
      { key: "Verification", fn: (m: AgentMetrics) => (m.verification_pass_rate ?? 0) * 100 },
      { key: "Low reopen", fn: (m: AgentMetrics) => Math.max(0, Math.round(100 - (m.reopen_rate ?? 0) * 20)) },
    ];
    return metricKeys.map(({ key, fn }) => {
      const row: Record<string, string | number> = { metric: key };
      for (const m of perRole) {
        row[BASE_ROLE_LABELS[m.base_role]] = Math.round(fn(m));
      }
      return row;
    });
  }, [perRole]);

  const radarKeys = useMemo(
    () => perRole.map((m) => BASE_ROLE_LABELS[m.base_role]),
    [perRole],
  );

  const radarColors = useMemo(
    () => perRole.map((m) => ROLE_COLORS[m.base_role]),
    [perRole],
  );

  // ── Token usage bar data ────────────────────────────────────────────────────
  const tokenBarData = useMemo(
    () =>
      perRole.map((m) => ({
        role: BASE_ROLE_LABELS[m.base_role],
        "Tokens in": Math.round((m.avg_tokens_in ?? 0) / 1000),
        "Tokens out": Math.round((m.avg_tokens_out ?? 0) / 1000),
      })),
    [perRole],
  );

  // ── Success rate over time (line chart) ─────────────────────────────────────
  const lineData = useMemo(
    () =>
      perRole
        .filter((m) => m.history.length >= 2)
        .map((m) => ({
          id: BASE_ROLE_LABELS[m.base_role],
          color: ROLE_COLORS[m.base_role],
          data: m.history.map((p) => ({
            x: p.date,
            y: Math.round(p.success_rate * 100),
          })),
        })),
    [perRole],
  );

  // ── Grouped by role (for detail cards) ──────────────────────────────────────
  const grouped = useMemo(
    () =>
      metrics
        ? BASE_ROLES.map((baseRole) => ({
            baseRole,
            label: BASE_ROLE_LABELS[baseRole],
            agents: metrics.filter((m) => m.base_role === baseRole),
          })).filter((g) => g.agents.length > 0)
        : [],
    [metrics],
  );

  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between shrink-0 gap-3">
        <div>
          <h2 className="text-lg font-semibold">Agent Metrics</h2>
          <p className="text-sm text-muted-foreground">
            Per-role effectiveness — refreshes every 30 s.
            {lastRefresh && (
              <span className="ml-2 text-xs">
                Last updated {lastRefresh.toLocaleTimeString()}
              </span>
            )}
          </p>
        </div>
        <Button
          variant="outline"
          size="sm"
          onClick={() => void load()}
          disabled={loading}
          className="shrink-0"
        >
          <HugeiconsIcon icon={Refresh01Icon} size={14} className={cn("mr-1.5", loading && "animate-spin")} />
          Refresh
        </Button>
      </div>

      {/* States */}
      {!projectId && (
        <div className="rounded-lg border border-border bg-card p-6 text-sm text-muted-foreground">
          Select a project to view metrics.
        </div>
      )}
      {error && <InlineError message={error} onRetry={() => void load()} />}
      {loading && !metrics && projectId && (
        <div className="rounded-lg border border-border bg-card p-6 text-sm text-muted-foreground">
          Loading metrics...
        </div>
      )}
      {metrics && metrics.length === 0 && (
        <div className="rounded-lg border border-dashed border-border p-8 text-center space-y-1">
          <p className="text-sm font-medium">No role metrics yet</p>
          <p className="text-xs text-muted-foreground">
            Metrics appear once agents complete tasks under a role.
          </p>
        </div>
      )}

      {/* Dashboard content */}
      {kpis && (
        <>
          {/* KPI strip */}
          <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
            <KpiCard
              label="Total tasks"
              value={String(kpis.totalTasks)}
              hint="Total closed tasks across all default roles"
            />
            <KpiCard
              label="Success rate"
              value={fmtPct(kpis.successRate)}
              trend={kpis.successTrend}
              hint="Weighted average: percentage of tasks completed successfully (not force-closed)"
            />
            <KpiCard
              label="Avg completion time"
              value={fmtDuration(kpis.avgTime)}
              hint="Weighted average time to complete a task"
            />
            <KpiCard
              label="Verification rate"
              value={fmtPct(kpis.verificationRate)}
              hint="Weighted average: percentage of tasks with zero verification failures across their lifetime"
            />
          </div>

          {/* Charts row: radar + token usage */}
          <div className="grid grid-cols-1 lg:grid-cols-2 gap-3">
            {radarData.length > 0 && radarKeys.length > 1 && (
              <ChartSection title="Role comparison" height={300}>
                <ResponsiveRadar
                  data={radarData}
                  keys={radarKeys}
                  indexBy="metric"
                  maxValue={100}
                  margin={{ top: 40, right: 80, bottom: 40, left: 80 }}
                  borderWidth={2}
                  borderColor={{ from: "color" }}
                  gridLevels={4}
                  gridShape="circular"
                  gridLabelOffset={20}
                  dotSize={6}
                  dotColor={{ theme: "background" }}
                  dotBorderWidth={2}
                  dotBorderColor={{ from: "color" }}
                  colors={radarColors}
                  fillOpacity={0.15}
                  blendMode="normal"
                  theme={nivoTheme}
                  legends={[
                    {
                      anchor: "top-left",
                      direction: "column",
                      translateX: -60,
                      translateY: -30,
                      itemWidth: 80,
                      itemHeight: 18,
                      itemTextColor: "oklch(0.705 0.015 286.067)",
                      symbolSize: 10,
                      symbolShape: "circle",
                    },
                  ]}
                />
              </ChartSection>
            )}

            {tokenBarData.length > 0 && (
              <ChartSection title="Avg token usage (K)" height={300}>
                <ResponsiveBar
                  data={tokenBarData}
                  keys={["Tokens in", "Tokens out"]}
                  indexBy="role"
                  groupMode="grouped"
                  margin={{ top: 10, right: 16, bottom: 40, left: 60 }}
                  padding={0.3}
                  innerPadding={2}
                  colors={[...TOKEN_BAR_COLORS]}
                  borderRadius={4}
                  axisBottom={{
                    tickSize: 0,
                    tickPadding: 8,
                  }}
                  axisLeft={{
                    tickSize: 0,
                    tickPadding: 8,
                  }}
                  enableLabel={false}
                  theme={nivoTheme}
                  legends={[
                    {
                      dataFrom: "keys",
                      anchor: "top-right",
                      direction: "row",
                      translateY: -4,
                      itemWidth: 90,
                      itemHeight: 18,
                      itemTextColor: "oklch(0.705 0.015 286.067)",
                      symbolSize: 10,
                      symbolShape: "circle",
                    },
                  ]}
                />
              </ChartSection>
            )}
          </div>

          {/* Success rate history line chart */}
          {lineData.length > 0 && (
            <ChartSection title="Success rate over time" height={260}>
              <ResponsiveLine
                data={lineData}
                margin={{ top: 10, right: 24, bottom: 40, left: 50 }}
                xScale={{ type: "point" }}
                yScale={{ type: "linear", min: 0, max: 100 }}
                curve="monotoneX"
                colors={lineData.map((d) => d.color)}
                lineWidth={2}
                pointSize={6}
                pointColor={{ theme: "background" }}
                pointBorderWidth={2}
                pointBorderColor={{ from: "serieColor" }}
                enableGridX={false}
                axisBottom={{
                  tickSize: 0,
                  tickPadding: 8,
                  tickRotation: -30,
                }}
                axisLeft={{
                  tickSize: 0,
                  tickPadding: 8,
                  format: (v: number) => `${v}%`,
                }}
                enableSlices="x"
                theme={nivoTheme}
                legends={[
                  {
                    anchor: "top-right",
                    direction: "row",
                    translateY: -4,
                    itemWidth: 80,
                    itemHeight: 18,
                    itemTextColor: "oklch(0.705 0.015 286.067)",
                    symbolSize: 10,
                    symbolShape: "circle",
                  },
                ]}
              />
            </ChartSection>
          )}
        </>
      )}

      {/* Per-role detail cards */}
      {grouped.map(({ baseRole, label, agents }) => (
        <div key={baseRole} className="space-y-2">
          <h3 className="text-sm font-semibold text-muted-foreground uppercase tracking-wide">
            {label}
          </h3>
          {agents.map((m) => (
            <RoleDetailCard key={m.agent_id} metrics={m} />
          ))}
        </div>
      ))}
    </div>
  );
}

// ── Role detail card ─────────────────────────────────────────────────────────

function RoleDetailCard({ metrics }: { metrics: AgentMetrics }) {
  const isEmpty = metrics.task_count === 0;
  const successPct = metrics.success_rate !== null ? Math.round(metrics.success_rate * 100) : null;

  return (
    <div className="rounded-lg border border-border bg-card p-4 space-y-3">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2 flex-wrap">
            <span className="font-medium truncate">{metrics.agent_name}</span>
            {metrics.is_default && (
              <span className="shrink-0 rounded-full bg-muted px-2 py-0.5 text-xs text-muted-foreground">
                default
              </span>
            )}
          </div>
          <p className="text-xs text-muted-foreground mt-0.5">
            {metrics.task_count === 0
              ? "No tasks yet"
              : `${metrics.task_count} task${metrics.task_count === 1 ? "" : "s"}`}
          </p>
        </div>
        <div className="flex items-center gap-1.5 shrink-0">
          <TrendIcon trend={metrics.success_rate_trend} />
        </div>
      </div>

      {isEmpty ? (
        <p className="text-xs text-muted-foreground italic">
          No task history — metrics will appear once this role completes tasks.
        </p>
      ) : (
        <div className="flex items-center gap-6 pt-1 border-t border-border">
          {/* Success gauge */}
          {successPct !== null && (
            <div
              className="flex flex-col items-center gap-1 shrink-0"
              title="Percentage of closed tasks that completed successfully (not force-closed or failed)"
            >
              <SuccessGauge value={successPct} color={ROLE_COLORS[metrics.base_role]} />
              <p className="text-xs text-muted-foreground">Success</p>
            </div>
          )}
          {/* Stats grid */}
          <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-5 gap-3 flex-1 min-w-0">
            <MetricCell label="Avg tokens in" value={fmtTokens(metrics.avg_tokens_in)} hint="Average input tokens per task" />
            <MetricCell label="Avg tokens out" value={fmtTokens(metrics.avg_tokens_out)} hint="Average output tokens per task" />
            <MetricCell label="Avg time" value={fmtDuration(metrics.avg_time_to_complete_seconds)} hint="Average time to complete a task" />
            <MetricCell label="Verification" value={fmtPct(metrics.verification_pass_rate)} hint="Percentage of tasks that passed verification with zero failures" />
            <MetricCell label="Avg reopens" value={fmtAvg(metrics.reopen_rate)} hint="Average number of times a task was reopened after closing" />
          </div>
        </div>
      )}
    </div>
  );
}

// ── Small metric cell ────────────────────────────────────────────────────────

function MetricCell({ label, value, hint }: { label: string; value: string; hint?: string }) {
  return (
    <div className="space-y-0.5" title={hint}>
      <p className="text-xs text-muted-foreground">{label}</p>
      <p className="text-sm font-medium tabular-nums">{value}</p>
    </div>
  );
}

// ── SVG success gauge ────────────────────────────────────────────────────────

function SuccessGauge({ value, color }: { value: number; color: string }) {
  const r = 28;
  const stroke = 5;
  const circumference = 2 * Math.PI * r;
  const progress = (value / 100) * circumference;

  return (
    <div className="relative flex items-center justify-center" style={{ width: 68, height: 68 }}>
      <svg width={68} height={68} className="-rotate-90">
        <circle
          cx={34}
          cy={34}
          r={r}
          fill="none"
          stroke="oklch(1 0 0 / 6%)"
          strokeWidth={stroke}
        />
        <circle
          cx={34}
          cy={34}
          r={r}
          fill="none"
          stroke={color}
          strokeWidth={stroke}
          strokeDasharray={circumference}
          strokeDashoffset={circumference - progress}
          strokeLinecap="round"
        />
      </svg>
      <span className="absolute text-sm font-semibold tabular-nums">{value}%</span>
    </div>
  );
}
