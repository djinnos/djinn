import { useMemo, useState } from "react";
import type {
  UsageAnalyticsResponse,
  UsageProjectModelCell,
} from "@/api/analytics";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import {
  EM_DASH,
  formatAverageReopens,
  formatCompactNumber,
  formatCurrency,
  formatInteger,
  formatPercent,
} from "./usageFormatters";

export type MatrixMetricKey =
  | "list_price_cost_per_task"
  | "success_rate"
  | "avg_reopens"
  | "actual_spend_usd"
  | "projected_usd"
  | "list_price_usd"
  | "total_tokens";

interface MatrixMetricSpec {
  key: MatrixMetricKey;
  label: string;
  description: string;
  format: (value: number | null | undefined) => string;
  emptyLabel: string;
  lowerIsDarker?: boolean;
}

const MATRIX_METRICS: MatrixMetricSpec[] = [
  {
    // Combined list-price cost per task (actual + projected) — the
    // apples-to-apples default across API-key and flat-rate-plan models.
    key: "list_price_cost_per_task",
    label: "Cost / task (list-price)",
    description:
      "Combined list-price cost (actual + projected) divided by credited worker tasks. Unpriced pairs render as —.",
    format: formatCurrency,
    emptyLabel: "No priced cost/task",
  },
  {
    key: "list_price_usd",
    label: "Total cost (list-price)",
    description:
      "Combined list-price cost (actual + projected) for the project/model pair.",
    format: formatCurrency,
    emptyLabel: "No priced cost",
  },
  {
    key: "actual_spend_usd",
    label: "Actual API spend",
    description:
      "Real API-key spend for the project/model pair. Subscription sessions contribute $0.",
    format: formatCurrency,
    emptyLabel: "No actual spend",
  },
  {
    key: "projected_usd",
    label: "Projected subscription-equivalent cost",
    description:
      "List-price equivalent for flat-rate plan sessions. API-key sessions contribute $0 here.",
    format: formatCurrency,
    emptyLabel: "No projected cost",
  },
  {
    key: "success_rate",
    label: "Success rate",
    description: "Completed-task success rate for the project/model pair.",
    format: formatPercent,
    emptyLabel: "No success-rate data",
    lowerIsDarker: false,
  },
  {
    key: "avg_reopens",
    label: "Avg reopens",
    description: "Average reopen count for credited worker tasks.",
    format: formatAverageReopens,
    emptyLabel: "No reopen data",
    lowerIsDarker: true,
  },
  {
    key: "total_tokens",
    label: "Tokens",
    description:
      "Input plus output tokens for the pair. Zero-token runs remain distinct from never-ran pairs.",
    format: formatCompactNumber,
    emptyLabel: "No token data",
  },
];

interface MatrixProject {
  id: string;
  name: string;
}

interface BuiltMatrix {
  projects: MatrixProject[];
  models: string[];
  cellsByPair: Map<string, UsageProjectModelCell>;
}

export function UsageProjectModelMatrixTab({
  data,
}: {
  data: UsageAnalyticsResponse;
}) {
  const [metricKey, setMetricKey] = useState<MatrixMetricKey>(
    "list_price_cost_per_task",
  );
  const metric = getMatrixMetric(metricKey);
  const matrix = useMemo(
    () => buildProjectModelMatrix(data.project_model_matrix),
    [data.project_model_matrix],
  );
  const scale = useMemo(
    () => buildMetricScale(data.project_model_matrix, metric),
    [data.project_model_matrix, metric],
  );

  if (data.project_model_matrix.length === 0) {
    return (
      <div className="rounded-lg border border-dashed border-border bg-card/50 px-4 py-8 text-center">
        <p className="text-sm font-medium text-foreground">
          No project/model usage
        </p>
        <p className="mt-1 text-sm text-muted-foreground">
          Matrix cells will appear once project/model pairs have worker usage in
          the selected filter range.
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
              Project × Model Matrix
            </h2>
            <p className="mt-1 max-w-3xl text-xs text-muted-foreground">
              Heatmap of project/model worker usage. Blank patterned cells mean
              the pair never ran in the selected filters; unpriced cost metrics
              render as — rather than $0, while token counts remain visible.
              Actual API spend and projected subscription-equivalent cost are
              available as separate metrics.
            </p>
          </div>
          <div className="flex items-center gap-2">
            <Label className="text-xs text-muted-foreground">Metric</Label>
            <Select
              value={metricKey}
              onValueChange={(value) => {
                if (isMatrixMetricKey(value)) setMetricKey(value);
              }}
            >
              <SelectTrigger
                className="h-8 w-[160px] text-xs"
                title="Matrix metric"
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {MATRIX_METRICS.map((option) => (
                  <SelectItem key={option.key} value={option.key}>
                    {option.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        </div>
        <div className="mt-3 flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
          <Badge variant="outline" className="text-muted-foreground">
            {formatInteger(matrix.projects.length)} projects
          </Badge>
          <Badge variant="outline" className="text-muted-foreground">
            {formatInteger(matrix.models.length)} models
          </Badge>
          <span>{metric.description}</span>
        </div>
      </div>

      <div className="overflow-x-auto">
        <table className="w-full min-w-[760px] border-separate border-spacing-0 text-left text-sm">
          <caption className="sr-only">
            Project by model heatmap for {metric.label}
          </caption>
          <thead className="text-xs text-muted-foreground">
            <tr>
              <th className="sticky left-0 z-10 border-b border-border bg-card px-4 py-3 font-medium">
                Project
              </th>
              {matrix.models.map((model) => (
                <th
                  key={model}
                  className="min-w-[150px] border-b border-border px-3 py-3 font-medium"
                >
                  <span className="block max-w-[180px] truncate" title={model}>
                    {model}
                  </span>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {matrix.projects.map((project) => (
              <tr key={project.id} className="align-top">
                <th className="sticky left-0 z-10 max-w-[220px] border-b border-border bg-card px-4 py-3 font-medium text-foreground">
                  <span className="block truncate" title={project.name}>
                    {project.name}
                  </span>
                  {project.id !== project.name && (
                    <span
                      className="block truncate text-xs font-normal text-muted-foreground"
                      title={project.id}
                    >
                      {project.id}
                    </span>
                  )}
                </th>
                {matrix.models.map((model) => {
                  const cell = matrix.cellsByPair.get(
                    pairKey(project.id, model),
                  );
                  return (
                    <ProjectModelCell
                      key={model}
                      cell={cell}
                      metric={metric}
                      scale={scale}
                      projectName={project.name}
                      model={model}
                    />
                  );
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function ProjectModelCell({
  cell,
  metric,
  scale,
  projectName,
  model,
}: {
  cell?: UsageProjectModelCell;
  metric: MatrixMetricSpec;
  scale: MetricScale;
  projectName: string;
  model: string;
}) {
  if (!cell) {
    return (
      <td
        className="h-full border-b border-border p-2"
        title={`${projectName} / ${model}: never ran`}
      >
        <div className="flex h-full min-h-24 items-center justify-center rounded-md border border-dashed border-border bg-muted/20 text-xs text-muted-foreground">
          <span className="sr-only">Never ran</span>
          <span aria-hidden>{EM_DASH}</span>
        </div>
      </td>
    );
  }

  const value = getMetricValue(cell, metric.key);
  const label = metric.format(value);
  const intensity = getMetricIntensity(value, scale, metric);
  const cached = cell.tokens_cached ?? 0;
  const unpricedLabel =
    cell.unpriced_count != null && cell.unpriced_count > 0
      ? `; ${cell.unpriced_count} unpriced`
      : "";
  const title = `${projectName} / ${model}: ${metric.label} ${label}; actual spend ${formatCurrency(
    cell.actual_spend_usd,
  )}; projected ${formatCurrency(cell.projected_usd)}${unpricedLabel}; tokens ${formatCompactNumber(cell.total_tokens)} (${formatCompactNumber(cached)} cached)`;

  return (
    <td className="h-full border-b border-border p-2" title={title}>
      <div
        className={cn(
          "flex h-full min-h-24 flex-col rounded-md border px-3 py-2 tabular-nums transition-colors",
          value === null || value === undefined
            ? "border-dashed border-border bg-muted/20"
            : "border-primary/20 bg-primary/10",
        )}
        style={
          value === null || value === undefined
            ? undefined
            : {
                backgroundColor: `color-mix(in oklch, var(--primary) ${intensity}%, transparent)`,
              }
        }
      >
        <div className="font-semibold text-foreground">{label}</div>
        {(value === null || value === undefined) && (
          <div className="mt-1 text-xs text-muted-foreground">
            {metric.emptyLabel}
          </div>
        )}
        <div className="mt-auto space-y-0.5 pt-3 text-xs text-muted-foreground">
          <div>Actual {formatCurrency(cell.actual_spend_usd)}</div>
          <div>Projected {formatCurrency(cell.projected_usd)}</div>
          {cell.unpriced_count != null && cell.unpriced_count > 0 && (
            <div>Unpriced {formatInteger(cell.unpriced_count)}</div>
          )}
          <div>
            Tokens {formatCompactNumber(cell.total_tokens)}
            {cached > 0 && ` · ${formatCompactNumber(cached)} cached`}
          </div>
        </div>
      </div>
    </td>
  );
}

interface MetricScale {
  min: number | null;
  max: number | null;
}

// eslint-disable-next-line react-refresh/only-export-components -- pure matrix builder colocated with the tab for its unit tests.
export function buildProjectModelMatrix(
  cells: UsageProjectModelCell[],
): BuiltMatrix {
  // Drop sessions with no project (chat/system runs carry an empty project_id
  // and would render as a nameless leading row), and drop any project row or
  // model column that recorded zero tokens across the whole window.
  const scoped = cells.filter((cell) => cell.project_id !== "");
  const projectTokens = new Map<string, number>();
  const modelTokens = new Map<string, number>();
  for (const cell of scoped) {
    projectTokens.set(
      cell.project_id,
      (projectTokens.get(cell.project_id) ?? 0) + cell.total_tokens,
    );
    modelTokens.set(
      cell.model,
      (modelTokens.get(cell.model) ?? 0) + cell.total_tokens,
    );
  }

  const projectsById = new Map<string, MatrixProject>();
  const models = new Set<string>();
  const cellsByPair = new Map<string, UsageProjectModelCell>();

  for (const cell of scoped) {
    if ((projectTokens.get(cell.project_id) ?? 0) === 0) continue;
    if ((modelTokens.get(cell.model) ?? 0) === 0) continue;
    projectsById.set(cell.project_id, {
      id: cell.project_id,
      name: cell.project_name || cell.project_id,
    });
    models.add(cell.model);
    cellsByPair.set(pairKey(cell.project_id, cell.model), cell);
  }

  return {
    projects: Array.from(projectsById.values()).sort((a, b) =>
      a.name.localeCompare(b.name),
    ),
    models: Array.from(models).sort((a, b) => a.localeCompare(b)),
    cellsByPair,
  };
}

// eslint-disable-next-line react-refresh/only-export-components -- pure formatter colocated with the tab for its unit tests.
export function formatMatrixMetricValue(
  cell: UsageProjectModelCell | undefined,
  metricKey: MatrixMetricKey,
): string {
  if (!cell) return EM_DASH;
  const metric = getMatrixMetric(metricKey);
  return metric.format(getMetricValue(cell, metricKey));
}

function buildMetricScale(
  cells: UsageProjectModelCell[],
  metric: MatrixMetricSpec,
): MetricScale {
  const values = cells
    .map((cell) => getMetricValue(cell, metric.key))
    .filter((value): value is number => value !== null && value !== undefined);
  return {
    min: values.length ? Math.min(...values) : null,
    max: values.length ? Math.max(...values) : null,
  };
}

function getMetricIntensity(
  value: number | null | undefined,
  scale: MetricScale,
  metric: MatrixMetricSpec,
): number {
  if (
    value === null ||
    value === undefined ||
    scale.min === null ||
    scale.max === null
  ) {
    return 0;
  }
  if (scale.min === scale.max) return 28;
  const normalized = (value - scale.min) / (scale.max - scale.min);
  const score = metric.lowerIsDarker ? 1 - normalized : normalized;
  return 10 + Math.round(score * 42);
}

function getMetricValue(
  cell: UsageProjectModelCell,
  metricKey: MatrixMetricKey,
): number | null {
  const val = cell[metricKey];
  return val ?? null;
}

function getMatrixMetric(key: MatrixMetricKey): MatrixMetricSpec {
  return (
    MATRIX_METRICS.find((metric) => metric.key === key) ?? MATRIX_METRICS[0]
  );
}

function isMatrixMetricKey(value: string | null): value is MatrixMetricKey {
  return MATRIX_METRICS.some((metric) => metric.key === value);
}

function pairKey(projectId: string, model: string): string {
  return `${projectId}\u0000${model}`;
}
