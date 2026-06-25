import { describe, expect, it } from "vitest";

import { buildUsageAnalyticsQueryString } from "@/api/analytics";
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
