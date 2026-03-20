import { useCallback, useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { InlineError } from "@/components/InlineError";
import { cn } from "@/lib/utils";
import { type BaseRole, type RoleMetrics, fetchRoleMetrics } from "@/api/roles";
import { TrendingUp, TrendingDown, Minus, RefreshCw } from "lucide-react";

const BASE_ROLE_LABELS: Record<BaseRole, string> = {
  worker: "Worker",
  task_reviewer: "Task Reviewer",
  pm: "Planner (PM)",
  groomer: "Groomer",
};

const BASE_ROLES: BaseRole[] = ["worker", "task_reviewer", "pm", "groomer"];

const POLL_INTERVAL_MS = 30_000;

// ── Formatters ────────────────────────────────────────────────────────────────

function fmtPct(value: number | null): string {
  if (value === null) return "—";
  return `${Math.round(value * 100)}%`;
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

// ── Trend indicator ───────────────────────────────────────────────────────────

function TrendIcon({ trend }: { trend: number | null }) {
  if (trend === null) return <Minus className="h-3.5 w-3.5 text-muted-foreground" />;
  if (trend > 0.01) return <TrendingUp className="h-3.5 w-3.5 text-green-500" />;
  if (trend < -0.01) return <TrendingDown className="h-3.5 w-3.5 text-red-500" />;
  return <Minus className="h-3.5 w-3.5 text-muted-foreground" />;
}

// ── Mini sparkline ────────────────────────────────────────────────────────────

function Sparkline({ points }: { points: number[] }) {
  if (points.length < 2) return null;
  const min = Math.min(...points);
  const max = Math.max(...points);
  const range = max - min || 1;
  const w = 60;
  const h = 20;
  const step = w / (points.length - 1);
  const coords = points
    .map((v, i) => `${i * step},${h - ((v - min) / range) * h}`)
    .join(" ");

  return (
    <svg width={w} height={h} className="opacity-60">
      <polyline
        points={coords}
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinejoin="round"
        strokeLinecap="round"
      />
    </svg>
  );
}

// ── Metric cell ───────────────────────────────────────────────────────────────

function MetricCell({ label, value }: { label: string; value: string }) {
  return (
    <div className="space-y-0.5">
      <p className="text-xs text-muted-foreground">{label}</p>
      <p className="text-sm font-medium tabular-nums">{value}</p>
    </div>
  );
}

// ── Role metrics card ─────────────────────────────────────────────────────────

function RoleMetricsCard({ metrics }: { metrics: RoleMetrics }) {
  const historyPoints = metrics.history.map((p) => p.success_rate);
  const isEmpty = metrics.task_count === 0;

  return (
    <div className="rounded-lg border border-border bg-card p-4 space-y-3">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2 flex-wrap">
            <span className="font-medium truncate">{metrics.role_name}</span>
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

        <div className="flex items-center gap-1.5 shrink-0 text-muted-foreground">
          <TrendIcon trend={metrics.success_rate_trend} />
          {historyPoints.length >= 2 && <Sparkline points={historyPoints} />}
        </div>
      </div>

      {isEmpty ? (
        <p className="text-xs text-muted-foreground italic">
          No task history — metrics will appear once this role completes tasks.
        </p>
      ) : (
        <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-5 gap-3 pt-1 border-t border-border">
          <MetricCell label="Success rate" value={fmtPct(metrics.success_rate)} />
          <MetricCell label="Avg tokens" value={fmtTokens(metrics.avg_token_usage)} />
          <MetricCell label="Avg time" value={fmtDuration(metrics.avg_time_to_complete_seconds)} />
          <MetricCell label="Verification ✓" value={fmtPct(metrics.verification_pass_rate)} />
          <MetricCell label="Reopen rate" value={fmtPct(metrics.reopen_rate)} />
        </div>
      )}
    </div>
  );
}

// ── Main component ────────────────────────────────────────────────────────────

export function AgentMetricsDashboard() {
  const [metrics, setMetrics] = useState<RoleMetrics[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [lastRefresh, setLastRefresh] = useState<Date | null>(null);
  const pollRef = useRef<number | null>(null);

  const load = useCallback(async (silent = false) => {
    if (!silent) setLoading(true);
    setError(null);
    try {
      const data = await fetchRoleMetrics();
      setMetrics(data.metrics);
      setLastRefresh(new Date());
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load metrics");
    } finally {
      if (!silent) setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
    pollRef.current = window.setInterval(() => void load(true), POLL_INTERVAL_MS);
    return () => {
      if (pollRef.current !== null) window.clearInterval(pollRef.current);
    };
  }, [load]);

  // Group by base role, specialists separate from defaults
  const grouped = metrics
    ? BASE_ROLES.map((baseRole) => ({
        baseRole,
        label: BASE_ROLE_LABELS[baseRole],
        defaults: metrics.filter((m) => m.base_role === baseRole && m.is_default),
        specialists: metrics.filter((m) => m.base_role === baseRole && !m.is_default),
      })).filter((g) => g.defaults.length > 0 || g.specialists.length > 0)
    : [];

  return (
    <div className="space-y-6">
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
          <RefreshCw className={cn("h-3.5 w-3.5 mr-1.5", loading && "animate-spin")} />
          Refresh
        </Button>
      </div>

      {error && (
        <InlineError
          message={error.includes("404") || error.includes("Failed to fetch")
            ? "Metrics endpoint not available yet — requires server task 32m9 (role_metrics)."
            : error}
          onRetry={() => void load()}
        />
      )}

      {loading && !metrics && (
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

      {grouped.map(({ baseRole, label, defaults, specialists }) => (
        <div key={baseRole} className="space-y-2">
          <h3 className="text-sm font-semibold text-muted-foreground uppercase tracking-wide">
            {label}
          </h3>

          {defaults.map((m) => (
            <RoleMetricsCard key={m.role_id} metrics={m} />
          ))}

          {specialists.length > 0 && (
            <>
              <p className="text-xs text-muted-foreground pl-0.5 pt-1">Specialists</p>
              {specialists.map((m) => (
                <RoleMetricsCard key={m.role_id} metrics={m} />
              ))}
            </>
          )}
        </div>
      ))}
    </div>
  );
}
