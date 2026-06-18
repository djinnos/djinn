import { getServerBaseUrl } from "@/api/serverUrl";

async function getBaseUrl(): Promise<string> {
  return getServerBaseUrl();
}

// ── Request types ──────────────────────────────────────────────────────────

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
}

// ── Response types ─────────────────────────────────────────────────────────

export interface UsageKpi {
  label: string;
  value: number | null;
  /** Period-over-period delta (e.g. +12% or -5%). */
  delta_pct: number | null;
  /** Formatted display value for the KPI card. */
  formatted: string;
}

export interface UsageTimeSeriesPoint {
  /** ISO-8601 date string for the bucket. */
  date: string;
  /** Spend amount in USD. May be null when pricing data is unavailable. */
  cost: number | null;
  tokens_in: number;
  tokens_out: number;
  task_count: number;
  /** Optional dimension key (model, project, agent_type) when grouped. */
  group_key?: string;
  /** Optional model dimension supplied by grouped overview responses. */
  model?: string;
  /** Optional project dimension supplied by grouped overview responses. */
  project_id?: string;
  project_name?: string;
  /** Optional agent role / type dimension supplied by grouped overview responses. */
  agent_type?: string;
  agent_role?: string;
}

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
}

export interface UsageBreakdownRow {
  id: string;
  name: string;
  cost: number | null;
  tokens_in: number;
  tokens_out: number;
  task_count: number;
  success_rate: number | null;
  avg_reopens: number | null;
  cost_per_task: number | null;
  model_split?: UsageModelSplit[];
  time_series?: UsageTimeSeriesPoint[];
  url?: string | null;
  task_id?: string | null;
  task_url?: string | null;
  proposal_id?: string | null;
  proposal_url?: string | null;
}

export interface UsageModelEffectiveness {
  model: string;
  task_count: number;
  success_rate: number | null;
  avg_reopens: number | null;
  cost_per_task: number | null;
  total_cost: number | null;
  total_tokens: number;
  tokens_in?: number;
  tokens_out?: number;
  session_count?: number;
  completed_task_count?: number;
  reopen_count?: number;
}

export interface UsageProjectModelCell {
  project_id: string;
  project_name: string;
  model: string;
  cost_per_task: number | null;
  success_rate: number | null;
  avg_reopens: number | null;
  total_cost: number | null;
  total_tokens: number;
}

/**
 * Top-level response shape for `GET /api/admin/usage`.
 * Each tab draws from its own key; tabs will be added in subsequent tasks.
 */
export interface UsageAnalyticsResponse {
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
  generated_at: string;
}

// ── Serialization ──────────────────────────────────────────────────────────

function buildQueryString(filters: UsageAnalyticsFilters): string {
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

  const qs = params.toString();
  return qs ? `?${qs}` : "";
}

// ── Fetch ──────────────────────────────────────────────────────────────────

export async function fetchUsageAnalytics(
  filters: UsageAnalyticsFilters = {},
): Promise<UsageAnalyticsResponse> {
  const baseUrl = await getBaseUrl();
  const qs = buildQueryString(filters);
  const response = await fetch(`${baseUrl}/api/admin/usage${qs}`);
  if (!response.ok) {
    const text = await response.text();
    throw new Error(
      `Failed to fetch usage analytics: ${text || response.status}`,
    );
  }
  return response.json() as Promise<UsageAnalyticsResponse>;
}
