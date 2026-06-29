import { getServerBaseUrl } from "@/api/serverUrl";

async function getBaseUrl(): Promise<string> {
  return getServerBaseUrl();
}

// ── Generated response types (source of truth) ───────────────────────────
//
// The canonical contract for `/api/admin/usage` response DTOs.
// Regenerate with `pnpm usage:types`; CI drift-check via `pnpm usage:types:check`.
import type {
  UsageResponse,
  UsageKpiDto,
  UsageKpiPartDto,
  SeriesPointDto,
  BreakdownsDto,
  BreakdownRowDto,
  ModelEffectivenessDto,
  ProjectModelCellDto,
} from "@/api/generated/usage-analytics.gen";

export type {
  UsageResponse,
  UsageKpiDto,
  UsageKpiPartDto,
  SeriesPointDto,
  BreakdownsDto,
  BreakdownRowDto,
  ModelEffectivenessDto,
  ProjectModelCellDto,
};

// ── Request types (locally authored) ─────────────────────────────────────

export type DateRangePreset = "7d" | "30d";

export type Granularity = "day" | "week" | "month";

/**
 * Global filter set for the `/api/admin/usage` endpoint.
 * Every tab in the usage dashboard applies the same set of filters so the
 * backend contract is shaped consistently.  `preset` and `start`/`end` are
 * mutually exclusive — when `preset` is set the date params are ignored.
 */
export interface UsageAnalyticsFilters {
  /** Shorthand date range; mutually exclusive with start/end. */
  preset?: DateRangePreset;
  /** ISO-8601 start date (inclusive), only used when preset is omitted. */
  start?: string;
  /** ISO-8601 end date (inclusive), only used when preset is omitted. */
  end?: string;
  /** Bucket granularity for time-series data. */
  granularity?: Granularity;
  /** Filter to a specific project ID. */
  project_id?: string;
  /** Filter to a specific model identifier. */
  model?: string;
  /** Filter to a specific agent base role / type. */
  agent_type?: string;
  /** Filter to a specific user (session creator, falling back to task creator). */
  user_id?: string;
}

// ── Backward-compatible response type aliases ────────────────────────────
//
// These extend the generated DTOs with legacy field names and nullable
// markers that existing UI components depend on.  Generated names are the
// canonical contract — these aliases keep existing imports working until
// callers are migrated to the generated names directly.
//
// Key divergences from the generated types:
//   - `value` / `delta_pct` are `number | null` (always serialized by serde)
//     rather than `number | undefined` (json-schema-to-typescript output).
//   - `unpriced_count` (legacy alias) vs `unpriced_session_count` (generated).
//   - `cost`, `cost_per_task`, `total_cost`, `model_split`, `time_series`,
//     `url`, `task_url`, `proposal_url`, `group_key`, `agent_role` are
//     legacy/UI-only fields not in the generated schema.
//   - Some generated-required fields (`tokens_cached`, `session_count`,
//     etc.) are optional here because the hand-maintained types had them
//     optional and existing test fixtures omit them.

export type UsageKpiPart = UsageKpiPartDto;

export type UsageKpi = Omit<UsageKpiDto, "value" | "delta_pct"> & {
  value: number | null;
  delta_pct: number | null;
};

export type UsageTimeSeriesPoint = Omit<
  SeriesPointDto,
  | "tokens_cached"
  | "model"
  | "project_id"
  | "project_name"
  | "agent_type"
  | "unpriced_session_count"
> & {
  tokens_cached?: number;
  model?: string;
  project_id?: string;
  project_name?: string;
  agent_type?: string;
  /** Legacy blended cost field. */
  cost?: number | null;
  /** Legacy alias — prefer unpriced_session_count. */
  unpriced_count?: number;
  /** Group key for dimension grouping. */
  group_key?: string;
  /** Agent role (alias for agent_type in some views). */
  agent_role?: string;
};

/** Per-model cost split detail row (not in generated schema). */
export interface UsageModelSplit {
  model: string;
  cost: number | null;
  tokens_in?: number;
  tokens_out?: number;
  total_tokens?: number;
  task_count?: number;
  success_rate?: number | null;
  avg_reopens?: number | null;
  cost_per_task?: number | null;
  /** Real API-key spend for this model split (split cost-basis). */
  actual_spend_usd?: number | null;
  /** Projected subscription-equivalent cost for this model split. */
  projected_usd?: number | null;
  /** Count of unpriced sessions in this model split. */
  unpriced_count?: number;
}

export type UsageBreakdownRow = Omit<
  BreakdownRowDto,
  "success_rate" | "avg_reopens" | "tokens_cached" | "unpriced_session_count"
> & {
  success_rate: number | null;
  avg_reopens: number | null;
  tokens_cached?: number;
  /** Legacy alias — prefer unpriced_session_count. */
  unpriced_count?: number;
  /** Legacy blended cost. */
  cost?: number | null;
  /** Legacy cost per task (generated has actual_cost_per_task). */
  cost_per_task?: number | null;
  /** Per-model split detail (not in schema). */
  model_split?: UsageModelSplit[];
  /** Time-series detail (not in schema). */
  time_series?: UsageTimeSeriesPoint[];
  /** Link fields not in schema. */
  url?: string | null;
  task_url?: string | null;
  proposal_url?: string | null;
};

export type UsageModelEffectiveness = Omit<
  ModelEffectivenessDto,
  | "success_rate"
  | "avg_reopens"
  | "tokens_cached"
  | "session_count"
  | "completed_task_count"
  | "unpriced_session_count"
  | "actual_cost_per_task"
> & {
  success_rate: number | null;
  avg_reopens: number | null;
  tokens_cached?: number;
  session_count?: number;
  completed_task_count?: number;
  /** Legacy alias — prefer unpriced_session_count. */
  unpriced_count?: number;
  /** Legacy total cost (blended). */
  total_cost?: number | null;
  /** Legacy cost per task (generated has actual_cost_per_task). */
  cost_per_task?: number | null;
  /** Legacy reopen count. */
  reopen_count?: number;
  actual_cost_per_task?: number | null;
};

export type UsageProjectModelCell = Omit<
  ProjectModelCellDto,
  | "success_rate"
  | "avg_reopens"
  | "tokens_cached"
  | "unpriced_session_count"
  | "actual_cost_per_task"
> & {
  success_rate: number | null;
  avg_reopens: number | null;
  tokens_cached?: number;
  /** Legacy alias — prefer unpriced_session_count. */
  unpriced_count?: number;
  /** Legacy total cost (blended). */
  total_cost?: number | null;
  /** Legacy cost per task (generated has actual_cost_per_task). */
  cost_per_task?: number | null;
  actual_cost_per_task?: number | null;
};

export type UsageAnalyticsResponse = Omit<
  UsageResponse,
  | "kpis"
  | "unpriced_session_count"
  | "time_series"
  | "breakdowns"
  | "model_effectiveness"
  | "project_model_matrix"
> & {
  kpis: UsageKpi[];
  time_series: UsageTimeSeriesPoint[];
  breakdowns: {
    by_user: UsageBreakdownRow[];
    by_project: UsageBreakdownRow[];
    by_proposal: UsageBreakdownRow[];
    by_task: UsageBreakdownRow[];
  };
  model_effectiveness: UsageModelEffectiveness[];
  project_model_matrix: UsageProjectModelCell[];
  /**
   * Total count of unpriced sessions across the entire filtered window.
   * Present on split cost-basis responses; absent on pre-split responses.
   */
  unpriced_session_count?: number;
};

// ── Serialization ──────────────────────────────────────────────────────────

export function buildUsageAnalyticsQueryString(filters: UsageAnalyticsFilters): string {
  const params = new URLSearchParams();

  if (filters.preset) {
    params.set("preset", filters.preset);
  } else {
    if (filters.start) params.set("start", filters.start);
    if (filters.end) params.set("end", filters.end);
  }

  if (filters.granularity) params.set("granularity", filters.granularity);
  if (filters.project_id) params.set("project_id", filters.project_id);
  if (filters.model) params.set("model", filters.model);
  if (filters.agent_type) params.set("agent_type", filters.agent_type);
  if (filters.user_id) params.set("user_id", filters.user_id);

  const qs = params.toString();
  return qs ? `?${qs}` : "";
}

// ── Fetch ──────────────────────────────────────────────────────────────────

export async function fetchUsageAnalytics(
  filters: UsageAnalyticsFilters = {},
): Promise<UsageAnalyticsResponse> {
  const baseUrl = await getBaseUrl();
  const qs = buildUsageAnalyticsQueryString(filters);
  const response = await fetch(`${baseUrl}/api/admin/usage${qs}`);
  if (!response.ok) {
    const text = await response.text();
    throw new Error(
      `Failed to fetch usage analytics: ${text || response.status}`,
    );
  }
  return response.json() as Promise<UsageAnalyticsResponse>;
}
