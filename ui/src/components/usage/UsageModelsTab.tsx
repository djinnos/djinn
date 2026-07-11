import { useMemo } from "react";
import type {
  UsageAnalyticsResponse,
  UsageModelEffectiveness,
} from "@/api/analytics";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import {
  EM_DASH,
  formatAverageReopens,
  formatCompactNumber,
  formatCurrency,
  formatInteger,
  formatPercent,
  totalTokens,
} from "./usageFormatters";

interface MetricSpec {
  key: "list_price_cost_per_task" | "success_rate" | "avg_reopens";
  label: string;
  format: (value: number | null | undefined) => string;
  lowerIsBetter: boolean;
}

const METRICS: MetricSpec[] = [
  {
    // Combined list-price cost per task (actual + projected) — the
    // apples-to-apples axis across API-key and flat-rate-plan models.
    key: "list_price_cost_per_task",
    label: "Cost / task",
    format: formatCurrency,
    lowerIsBetter: true,
  },
  {
    key: "success_rate",
    label: "Success rate",
    format: formatPercent,
    lowerIsBetter: false,
  },
  {
    key: "avg_reopens",
    label: "Avg reopens",
    format: formatAverageReopens,
    lowerIsBetter: true,
  },
];

export function UsageModelsTab({ data }: { data: UsageAnalyticsResponse }) {
  // Hide models that recorded no tokens at all (e.g. a model whose only runs
  // were empty/failed sessions) — they add noise with nothing to compare.
  const models = useMemo(
    () =>
      data.model_effectiveness.filter(
        (row) =>
          totalTokens(row.tokens_in, row.tokens_out, row.total_tokens) > 0,
      ),
    [data],
  );
  const stats = useMemo(() => buildMetricStats(models), [models]);

  if (models.length === 0) {
    return (
      <div className="rounded-lg border border-dashed border-border bg-card/50 px-4 py-8 text-center">
        <p className="text-sm font-medium text-foreground">
          No worker model data
        </p>
        <p className="mt-1 text-sm text-muted-foreground">
          Worker-scoped model effectiveness appears once worker sessions
          complete in the selected range.
        </p>
      </div>
    );
  }

  return (
    <section className="rounded-lg border border-border bg-card">
      <div className="border-b border-border px-4 py-3">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <h2 className="text-sm font-medium text-foreground">
              Worker model effectiveness
            </h2>
            <p className="mt-1 max-w-3xl text-xs text-muted-foreground">
              Compares models using worker sessions/results only. Completed
              tasks are attributed to every model that ran a worker session on
              them, so task counts use shared credit rather than exclusive
              ownership. Because shared credit can flatter a model whose first
              passes are reworked by another, the <em>First-pass rejections</em>{" "}
              and <em>Final-pass share</em> columns discriminate who produced
              rejected passes from who actually landed the merge. Actual API
              spend and projected subscription-equivalent cost are shown
              separately.
            </p>
          </div>
          <Badge variant="outline" className="text-muted-foreground">
            {formatInteger(models.length)} models
          </Badge>
        </div>
      </div>

      <div className="overflow-x-auto">
        <table className="w-full min-w-[1360px] text-left text-sm">
          <thead className="border-b border-border text-xs text-muted-foreground">
            <tr>
              <th className="px-4 py-3 font-medium">Model</th>
              {METRICS.map((metric) => (
                <th key={metric.key} className="px-4 py-3 font-medium">
                  {metric.label}
                </th>
              ))}
              <th
                className="px-4 py-3 font-medium"
                title="Share of this model's worker sessions whose pass did not land: a later worker session reworked the task after it was reopened. High values mean weak first passes that shared-credit success rate hides. Lower is better."
              >
                First-pass rejections
              </th>
              <th
                className="px-4 py-3 font-medium"
                title="Share of this model's completed-task credits where it ran the LAST worker session before the task closed — i.e. it actually landed the merge (vs. inheriting shared credit for a task another model finished). Higher is better."
              >
                Final-pass share
              </th>
              <th className="px-4 py-3 font-medium">Tasks</th>
              <th className="px-4 py-3 font-medium">Sessions</th>
              <th className="px-4 py-3 font-medium">Tokens</th>
              <th className="px-4 py-3 font-medium">Actual API spend</th>
              <th className="px-4 py-3 font-medium">Projected subscription-equivalent cost</th>
              <th className="px-4 py-3 font-medium">Unpriced</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-border">
            {[...models]
              .sort(
                (a, b) =>
                  (b.task_count ?? 0) - (a.task_count ?? 0) ||
                  a.model.localeCompare(b.model),
              )
              .map((row) => (
                <tr key={row.model} className="align-top hover:bg-muted/30">
                  <td className="max-w-[260px] px-4 py-3">
                    <div
                      className="truncate font-medium text-foreground"
                      title={row.model}
                    >
                      {row.model}
                    </div>
                  </td>
                  {METRICS.map((metric) => (
                    <MetricCell
                      key={metric.key}
                      row={row}
                      metric={metric}
                      stats={stats[metric.key]}
                    />
                  ))}
                  <td
                    className="px-4 py-3 tabular-nums text-foreground"
                    title={
                      row.first_pass_rejected_session_count != null &&
                      row.session_count != null
                        ? `${formatInteger(
                            row.first_pass_rejected_session_count,
                          )} of ${formatInteger(
                            row.session_count,
                          )} worker sessions reworked after reopen`
                        : undefined
                    }
                  >
                    {formatPercent(row.first_pass_rejection_rate)}
                  </td>
                  <td
                    className="px-4 py-3 tabular-nums text-foreground"
                    title={
                      row.final_pass_completed_task_count != null &&
                      row.completed_task_count != null
                        ? `landed ${formatInteger(
                            row.final_pass_completed_task_count,
                          )} of ${formatInteger(
                            row.completed_task_count,
                          )} credited completed tasks`
                        : undefined
                    }
                  >
                    {formatPercent(row.final_pass_share)}
                  </td>
                  <td className="px-4 py-3 tabular-nums text-foreground">
                    <div>{formatInteger(row.task_count)}</div>
                    {row.completed_task_count !== undefined && (
                      <div className="text-xs text-muted-foreground">
                        {formatInteger(row.completed_task_count)} completed
                      </div>
                    )}
                  </td>
                  <td className="px-4 py-3 tabular-nums text-foreground">
                    {formatInteger(row.session_count)}
                  </td>
                  <td className="px-4 py-3 tabular-nums text-foreground">
                    <div>
                      {formatCompactNumber(
                        totalTokens(
                          row.tokens_in,
                          row.tokens_out,
                          row.total_tokens,
                        ),
                      )}
                    </div>
                    {(row.tokens_in !== undefined ||
                      row.tokens_out !== undefined) && (
                      <div className="text-xs text-muted-foreground">
                        {formatCompactNumber(row.tokens_in ?? 0)} in ·{" "}
                        {formatCompactNumber(row.tokens_cached ?? 0)} cached ·{" "}
                        {formatCompactNumber(row.tokens_out ?? 0)} out
                      </div>
                    )}
                  </td>
                  <td className="px-4 py-3 tabular-nums text-foreground">
                    {formatCurrency(row.actual_spend_usd)}
                  </td>
                  <td className="px-4 py-3 tabular-nums text-foreground">
                    {formatCurrency(row.projected_usd)}
                  </td>
                  <td className="px-4 py-3 tabular-nums text-foreground">
                    {row.unpriced_count != null && row.unpriced_count > 0
                      ? formatInteger(row.unpriced_count)
                      : EM_DASH}
                  </td>
                </tr>
              ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function MetricCell({
  row,
  metric,
  stats,
}: {
  row: UsageModelEffectiveness;
  metric: MetricSpec;
  stats: MetricStats;
}) {
  const value = row[metric.key];
  const isBest = value !== null && value !== undefined && value === stats.best;
  const barPct = getBarPct(value, stats, metric.lowerIsBetter);

  return (
    <td
      className={cn(
        "px-4 py-3 tabular-nums",
        isBest ? "bg-primary/10 text-primary" : "text-foreground",
      )}
      title={isBest ? "Best in column" : undefined}
    >
      <div className="flex items-center justify-between gap-2">
        <span className="font-medium">{metric.format(value)}</span>
        {isBest && (
          <Badge variant="secondary" className="h-4 px-1.5 text-[10px]">
            Best
          </Badge>
        )}
      </div>
      {value !== null && value !== undefined && (
        <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-muted">
          <div
            className={cn(
              "h-full rounded-full",
              isBest ? "bg-primary" : "bg-muted-foreground/50",
            )}
            style={{ width: `${barPct}%` }}
          />
        </div>
      )}
    </td>
  );
}

interface MetricStats {
  best: number | null;
  min: number | null;
  max: number | null;
}

type MetricStatsMap = Record<MetricSpec["key"], MetricStats>;

function buildMetricStats(rows: UsageModelEffectiveness[]): MetricStatsMap {
  return METRICS.reduce((acc, metric) => {
    const values = rows
      .map((row) => row[metric.key])
      .filter(
        (value): value is number => value !== null && value !== undefined,
      );
    const min = values.length ? Math.min(...values) : null;
    const max = values.length ? Math.max(...values) : null;
    acc[metric.key] = {
      min,
      max,
      best: metric.lowerIsBetter ? min : max,
    };
    return acc;
  }, {} as MetricStatsMap);
}

function getBarPct(
  value: number | null | undefined,
  stats: MetricStats,
  lowerIsBetter: boolean,
): number {
  if (
    value === null ||
    value === undefined ||
    stats.min === null ||
    stats.max === null
  ) {
    return 0;
  }
  if (stats.max === stats.min) return 100;
  const normalized = (value - stats.min) / (stats.max - stats.min);
  const score = lowerIsBetter ? 1 - normalized : normalized;
  return Math.max(8, Math.round(score * 100));
}
