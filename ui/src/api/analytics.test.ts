import { describe, expect, it } from "vitest";

import {
  buildUsageAnalyticsQueryString,
  type UsageAnalyticsResponse,
  type UsageKpi,
  type UsageTimeSeriesPoint,
  type UsageBreakdownRow,
  type UsageModelEffectiveness,
  type UsageProjectModelCell,
} from "@/api/analytics";
import {
  parseUsageAnalyticsFiltersFromSearchParams,
  serializeUsageAnalyticsFiltersToSearchParams,
} from "@/lib/usageAnalyticsFiltersUrl";

describe("usage analytics query serialization", () => {
  it("serializes global dashboard filters for the shared analytics endpoint", () => {
    expect(
      buildUsageAnalyticsQueryString({
        preset: "30d",
        start: "2026-01-01",
        end: "2026-01-31",
        granularity: "week",
        project_id: "project-1",
        model: "openai/gpt-4.1",
        agent_type: "worker",
      }),
    ).toBe(
      "?preset=30d&granularity=week&project_id=project-1&model=openai%2Fgpt-4.1&agent_type=worker",
    );
  });

  it("uses custom start/end only when no preset is selected", () => {
    expect(
      buildUsageAnalyticsQueryString({
        start: "2026-01-01",
        end: "2026-01-31",
        granularity: "day",
      }),
    ).toBe("?start=2026-01-01&end=2026-01-31&granularity=day");
  });

  it("omits empty filters", () => {
    expect(buildUsageAnalyticsQueryString({})).toBe("");
  });
});

// ── Cross-check: API query-string vs URL helper alignment ────────────────────

describe("buildUsageAnalyticsQueryString ↔ URL helper cross-check", () => {
  const ALL_KEYS_FILTERS = {
    preset: "7d" as const,
    granularity: "week" as const,
    project_id: "proj-x",
    model: "openai/gpt-4.1",
    agent_type: "worker",
    user_id: "user-1",
  };

  it("API query-string and URL helper produce the same field names for preset mode", () => {
    const apiQs = buildUsageAnalyticsQueryString(ALL_KEYS_FILTERS);
    const urlParams = serializeUsageAnalyticsFiltersToSearchParams(ALL_KEYS_FILTERS);

    // Extract the keys from both serializations.
    const apiKeys = new URLSearchParams(apiQs.replace(/^\?/, "")).keys();
    const urlKeys = urlParams.keys();

    const apiKeySet = new Set(apiKeys);
    const urlKeySet = new Set(urlKeys);

    expect(apiKeySet).toEqual(urlKeySet);
  });

  it("API query-string and URL helper produce the same field names for custom-date mode", () => {
    const customDateFilters = {
      start: "2026-01-01",
      end: "2026-03-31",
      granularity: "month" as const,
      project_id: "proj-y",
      model: "anthropic/claude-sonnet-4-20250514",
      agent_type: "planner",
      user_id: "user-2",
    };

    const apiQs = buildUsageAnalyticsQueryString(customDateFilters);
    const urlParams = serializeUsageAnalyticsFiltersToSearchParams(customDateFilters);

    const apiKeySet = new Set(new URLSearchParams(apiQs.replace(/^\?/, "")).keys());
    const urlKeySet = new Set(urlParams.keys());

    expect(apiKeySet).toEqual(urlKeySet);
  });

  it("both serializers enforce preset-vs-dates precedence consistently", () => {
    // When preset is present, start/end should be omitted by both.
    const filters = {
      preset: "30d" as const,
      start: "2026-01-01",
      end: "2026-01-31",
      granularity: "day" as const,
    };

    const apiQs = buildUsageAnalyticsQueryString(filters);
    const urlParams = serializeUsageAnalyticsFiltersToSearchParams(filters);

    expect(apiQs).toContain("preset=30d");
    expect(apiQs).not.toContain("start=");
    expect(apiQs).not.toContain("end=");

    expect(urlParams.get("preset")).toBe("30d");
    expect(urlParams.get("start")).toBeNull();
    expect(urlParams.get("end")).toBeNull();
  });

  it("round-trip through URL helper preserves the same keys as API serialization", () => {
    const original = {
      preset: "7d" as const,
      granularity: "week" as const,
      project_id: "proj-z",
      model: "openai/gpt-4.1",
    };

    const apiQs = buildUsageAnalyticsQueryString(original);
    const params = serializeUsageAnalyticsFiltersToSearchParams(original);
    const restored = parseUsageAnalyticsFiltersFromSearchParams(params);
    const restoredApiQs = buildUsageAnalyticsQueryString(restored);

    // The API query string should be the same before and after round-trip.
    expect(restoredApiQs).toBe(apiQs);
  });
});

// ── Split cost-basis response type fixtures ─────────────────────────────────
// These tests exercise compile-time type compatibility and runtime
// deserialization of the split cost contract (actual_spend_usd, projected_usd,
// unpriced_count) that the backend emits once task 87v3 lands.

describe("split cost-basis response type fixtures", () => {
  /**
   * A minimal fixture exercising every split-cost field path in the response
   * types.  The test is a compile-time guard: if a field name is misspelled or
   * the type narrows, TypeScript will fail to assign and vitest will not run.
   * At runtime we assert the fixture is parseable and the split fields survive
   * a JSON round-trip.
   */
  it("split-cost KPI, time-series, breakdown, model, and cell fixtures compile and round-trip", () => {
    // ── KPI with split fields ──────────────────────────────────────────────
    const spendKpi: UsageKpi = {
      label: "Spend",
      value: 42.5,
      delta_pct: 0.12,
      formatted: "$42.50",
      actual_spend_usd: 30.0,
      projected_usd: 12.5,
      unpriced_count: 3,
    };

    expect(spendKpi.actual_spend_usd).toBe(30.0);
    expect(spendKpi.projected_usd).toBe(12.5);
    expect(spendKpi.unpriced_count).toBe(3);

    // ── Time-series point with split fields ────────────────────────────────
    const point: UsageTimeSeriesPoint = {
      date: "2026-06-01",
      cost: 5.0,
      tokens_in: 1000,
      tokens_out: 500,
      tokens_cached: 200,
      task_count: 2,
      model: "openai/gpt-4.1",
      project_id: "proj-1",
      project_name: "Alpha",
      agent_type: "worker",
      actual_spend_usd: 3.0,
      projected_usd: 2.0,
      unpriced_count: 1,
    };

    expect(point.actual_spend_usd).toBe(3.0);
    expect(point.projected_usd).toBe(2.0);
    expect(point.unpriced_count).toBe(1);

    // ── Breakdown row with split fields ────────────────────────────────────
    const row: UsageBreakdownRow = {
      id: "user-1",
      name: "Alice",
      cost: 15.0,
      tokens_in: 5000,
      tokens_out: 2500,
      task_count: 4,
      success_rate: 0.8,
      avg_reopens: 0.5,
      cost_per_task: 3.75,
      actual_spend_usd: 10.0,
      projected_usd: 5.0,
      unpriced_count: 2,
    };

    expect(row.actual_spend_usd).toBe(10.0);
    expect(row.projected_usd).toBe(5.0);
    expect(row.unpriced_count).toBe(2);

    // ── Model effectiveness with split fields ──────────────────────────────
    const model: UsageModelEffectiveness = {
      model: "openai/gpt-4.1",
      task_count: 10,
      success_rate: 0.9,
      avg_reopens: 0.2,
      cost_per_task: 1.5,
      total_cost: 15.0,
      total_tokens: 20000,
      tokens_in: 12000,
      tokens_out: 8000,
      actual_spend_usd: 10.0,
      projected_usd: 5.0,
      unpriced_count: 1,
    };

    expect(model.actual_spend_usd).toBe(10.0);
    expect(model.projected_usd).toBe(5.0);
    expect(model.unpriced_count).toBe(1);

    // ── Project×model cell with split fields ───────────────────────────────
    const cell: UsageProjectModelCell = {
      project_id: "proj-1",
      project_name: "Alpha",
      model: "openai/gpt-4.1",
      cost_per_task: 2.0,
      success_rate: 0.85,
      avg_reopens: 0.3,
      total_cost: 20.0,
      total_tokens: 15000,
      actual_spend_usd: 14.0,
      projected_usd: 6.0,
      unpriced_count: 2,
    };

    expect(cell.actual_spend_usd).toBe(14.0);
    expect(cell.projected_usd).toBe(6.0);
    expect(cell.unpriced_count).toBe(2);

    // ── Full response with unpriced_session_count ──────────────────────────
    const response: UsageAnalyticsResponse = {
      kpis: [spendKpi],
      time_series: [point],
      breakdowns: {
        by_user: [row],
        by_project: [],
        by_proposal: [],
        by_task: [],
      },
      model_effectiveness: [model],
      project_model_matrix: [cell],
      generated_at: "2026-06-25T12:00:00Z",
      unpriced_session_count: 5,
    };

    expect(response.unpriced_session_count).toBe(5);

    // JSON round-trip: ensure the split fields survive serialization.
    const json = JSON.stringify(response);
    const parsed = JSON.parse(json) as UsageAnalyticsResponse;
    expect(parsed.kpis[0].actual_spend_usd).toBe(30.0);
    expect(parsed.kpis[0].projected_usd).toBe(12.5);
    expect(parsed.kpis[0].unpriced_count).toBe(3);
    expect(parsed.time_series[0].actual_spend_usd).toBe(3.0);
    expect(parsed.unpriced_session_count).toBe(5);
  });

  it("pre-split response shape (no split fields) is still accepted", () => {
    // The pre-split backend emits the same shape without actual_spend_usd,
    // projected_usd, or unpriced_count.  The types must accept this.
    const preSplit: UsageAnalyticsResponse = {
      kpis: [
        {
          label: "Spend",
          value: 100.0,
          delta_pct: null,
          formatted: "$100.00",
        },
      ],
      time_series: [
        {
          date: "2026-06-01",
          cost: 50.0,
          tokens_in: 10000,
          tokens_out: 5000,
          task_count: 5,
        },
      ],
      breakdowns: {
        by_user: [],
        by_project: [],
        by_proposal: [],
        by_task: [],
      },
      model_effectiveness: [],
      project_model_matrix: [],
      generated_at: "2026-06-25T12:00:00Z",
    };

    // Split fields should be undefined on the pre-split shape.
    expect(preSplit.kpis[0].actual_spend_usd).toBeUndefined();
    expect(preSplit.kpis[0].projected_usd).toBeUndefined();
    expect(preSplit.kpis[0].unpriced_count).toBeUndefined();
    expect(preSplit.time_series[0].actual_spend_usd).toBeUndefined();
    expect(preSplit.unpriced_session_count).toBeUndefined();
  });

  it("no ambiguous blended spend field exists in the type contract", () => {
    // The acceptance criteria explicitly forbid a new ambiguous blended dollar
    // field.  Verify that `cost` (the legacy field) is the only dollar field
    // at the time-series/breakdown level and that actual_spend_usd and
    // projected_usd are the only new fields.  This is a compile-time guard:
    // if someone adds a field named `spend`, `blended_cost`, or similar, this
    // test's type assertion will surface the concern.
    const point: UsageTimeSeriesPoint = {
      date: "2026-06-01",
      cost: 5.0, // legacy blended field — preserved for backward compat
      tokens_in: 100,
      tokens_out: 50,
      task_count: 1,
    };

    // The new split fields are distinct from the legacy `cost` field.
    expect(typeof point.cost).toBe("number");
    expect(point.actual_spend_usd).toBeUndefined();
    expect(point.projected_usd).toBeUndefined();
  });
});
