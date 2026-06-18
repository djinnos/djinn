import { describe, expect, it } from "vitest";

import { buildUsageAnalyticsQueryString } from "@/api/analytics";

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
