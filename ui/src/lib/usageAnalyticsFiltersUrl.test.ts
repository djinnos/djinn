import { describe, expect, it } from "vitest";

import type { UsageAnalyticsFilters } from "@/api/analytics";
import {
  parseUsageAnalyticsFiltersFromSearchParams,
  serializeUsageAnalyticsFiltersToSearchParams,
  DEFAULT_USAGE_FILTERS,
} from "@/lib/usageAnalyticsFiltersUrl";

// ── Parse ───────────────────────────────────────────────────────────────────

describe("parseUsageAnalyticsFiltersFromSearchParams", () => {
  // ── Defaults ──────────────────────────────────────────────────────────────

  it("returns last-30-days daily defaults when no params are present", () => {
    const params = new URLSearchParams();
    expect(parseUsageAnalyticsFiltersFromSearchParams(params)).toEqual({
      preset: "30d",
      granularity: "day",
    });
  });

  it("returns defaults when params are present but irrelevant", () => {
    const params = new URLSearchParams({ foo: "bar", page: "2" });
    expect(parseUsageAnalyticsFiltersFromSearchParams(params)).toEqual({
      preset: "30d",
      granularity: "day",
    });
  });

  // ── Valid preset restore ──────────────────────────────────────────────────

  it("restores a 7d preset", () => {
    const params = new URLSearchParams({ preset: "7d" });
    expect(parseUsageAnalyticsFiltersFromSearchParams(params)).toEqual({
      preset: "7d",
      granularity: "day",
    });
  });

  it("restores a 30d preset with custom granularity", () => {
    const params = new URLSearchParams({ preset: "30d", granularity: "week" });
    expect(parseUsageAnalyticsFiltersFromSearchParams(params)).toEqual({
      preset: "30d",
      granularity: "week",
    });
  });

  // ── Invalid enum filtering ────────────────────────────────────────────────

  it("drops invalid preset values and applies default", () => {
    const params = new URLSearchParams({ preset: "90d" });
    // 90d is not a valid preset, so the parser treats it as absent.
    // Since no start/end are provided either, preset defaults to 30d.
    expect(parseUsageAnalyticsFiltersFromSearchParams(params)).toEqual({
      preset: "30d",
      granularity: "day",
    });
  });

  it("drops invalid granularity values and applies default", () => {
    const params = new URLSearchParams({ granularity: "hour" });
    expect(parseUsageAnalyticsFiltersFromSearchParams(params)).toEqual({
      preset: "30d",
      granularity: "day",
    });
  });

  it("drops both invalid preset and invalid granularity", () => {
    const params = new URLSearchParams({ preset: "abc", granularity: "year" });
    expect(parseUsageAnalyticsFiltersFromSearchParams(params)).toEqual({
      preset: "30d",
      granularity: "day",
    });
  });

  // ── Preset precedence over dates ──────────────────────────────────────────

  it("ignores start/end when a valid preset is present", () => {
    const params = new URLSearchParams({
      preset: "7d",
      start: "2026-01-01",
      end: "2026-01-31",
    });
    const result = parseUsageAnalyticsFiltersFromSearchParams(params);
    expect(result.preset).toBe("7d");
    expect(result.start).toBeUndefined();
    expect(result.end).toBeUndefined();
  });

  // ── Custom dates without preset ───────────────────────────────────────────

  it("uses start/end when no valid preset is present and omits preset field", () => {
    const params = new URLSearchParams({
      start: "2026-01-01",
      end: "2026-01-31",
    });
    const result = parseUsageAnalyticsFiltersFromSearchParams(params);
    expect(result).toEqual({
      granularity: "day",
      start: "2026-01-01",
      end: "2026-01-31",
    });
    expect(result.preset).toBeUndefined();
  });

  it("uses only start when end is omitted", () => {
    const params = new URLSearchParams({ start: "2026-06-01" });
    const result = parseUsageAnalyticsFiltersFromSearchParams(params);
    expect(result).toEqual({
      granularity: "day",
      start: "2026-06-01",
    });
    expect(result.preset).toBeUndefined();
  });

  it("uses only end when start is omitted", () => {
    const params = new URLSearchParams({ end: "2026-06-30" });
    const result = parseUsageAnalyticsFiltersFromSearchParams(params);
    expect(result).toEqual({
      granularity: "day",
      end: "2026-06-30",
    });
    expect(result.preset).toBeUndefined();
  });

  // ── All supported URL keys ────────────────────────────────────────────────

  it("restores all supported URL keys together", () => {
    const params = new URLSearchParams({
      preset: "7d",
      granularity: "week",
      project_id: "proj-abc",
      model: "openai/gpt-4.1",
      agent_type: "worker",
      user_id: "user-123",
    });
    expect(parseUsageAnalyticsFiltersFromSearchParams(params)).toEqual({
      preset: "7d",
      granularity: "week",
      project_id: "proj-abc",
      model: "openai/gpt-4.1",
      agent_type: "worker",
      user_id: "user-123",
    });
  });

  it("restores all supported keys with custom dates (no preset)", () => {
    const params = new URLSearchParams({
      start: "2026-01-01",
      end: "2026-03-31",
      granularity: "month",
      project_id: "proj-xyz",
      model: "anthropic/claude-sonnet-4-20250514",
      agent_type: "planner",
      user_id: "user-456",
    });
    expect(parseUsageAnalyticsFiltersFromSearchParams(params)).toEqual({
      granularity: "month",
      start: "2026-01-01",
      end: "2026-03-31",
      project_id: "proj-xyz",
      model: "anthropic/claude-sonnet-4-20250514",
      agent_type: "planner",
      user_id: "user-456",
    });
  });

  // ── Empty-string handling in parse ─────────────────────────────────────────

  it("treats empty-string values as absent", () => {
    const params = new URLSearchParams({
      preset: "",
      granularity: "",
      project_id: "",
      model: "",
    });
    expect(parseUsageAnalyticsFiltersFromSearchParams(params)).toEqual({
      preset: "30d",
      granularity: "day",
    });
  });

  // ── Preset with invalid start/end (invalid preset → dates come through) ──

  it("passes start/end through when preset is invalid", () => {
    const params = new URLSearchParams({
      preset: "invalid",
      start: "2026-01-01",
      end: "2026-01-31",
    });
    const result = parseUsageAnalyticsFiltersFromSearchParams(params);
    expect(result.preset).toBeUndefined();
    expect(result.start).toBe("2026-01-01");
    expect(result.end).toBe("2026-01-31");
  });
});

// ── Serialize ───────────────────────────────────────────────────────────────

describe("serializeUsageAnalyticsFiltersToSearchParams", () => {
  // ── Empty field omission ──────────────────────────────────────────────────

  it("produces empty params for empty filter object", () => {
    const result = serializeUsageAnalyticsFiltersToSearchParams({});
    expect(result.toString()).toBe("");
  });

  it("omits undefined and empty-string fields", () => {
    const result = serializeUsageAnalyticsFiltersToSearchParams({
      preset: "30d",
      granularity: undefined,
      project_id: "",
      model: undefined,
      agent_type: "",
      user_id: undefined,
    });
    expect(result.toString()).toBe("preset=30d");
  });

  // ── Preset precedence over dates ──────────────────────────────────────────

  it("omits start/end when a valid preset is present", () => {
    const result = serializeUsageAnalyticsFiltersToSearchParams({
      preset: "7d",
      start: "2026-01-01",
      end: "2026-01-31",
    });
    expect(result.get("preset")).toBe("7d");
    expect(result.get("start")).toBeNull();
    expect(result.get("end")).toBeNull();
  });

  // ── Custom dates without preset ───────────────────────────────────────────

  it("includes start/end when no valid preset is set", () => {
    const result = serializeUsageAnalyticsFiltersToSearchParams({
      start: "2026-01-01",
      end: "2026-01-31",
      granularity: "day",
    });
    expect(result.get("start")).toBe("2026-01-01");
    expect(result.get("end")).toBe("2026-01-31");
    expect(result.get("preset")).toBeNull();
  });

  it("serializes only start when end is omitted", () => {
    const result = serializeUsageAnalyticsFiltersToSearchParams({
      start: "2026-06-01",
    });
    expect(result.get("start")).toBe("2026-06-01");
    expect(result.get("end")).toBeNull();
    expect(result.get("preset")).toBeNull();
  });

  // ── No leading `?` ────────────────────────────────────────────────────────

  it("returns URLSearchParams without a leading question mark", () => {
    const result = serializeUsageAnalyticsFiltersToSearchParams({
      preset: "30d",
      granularity: "week",
    });
    const str = result.toString();
    expect(str).not.toMatch(/^\?/);
  });

  // ── Stable key ordering via URLSearchParams ────────────────────────────────

  it("produces stable param string for identical input", () => {
    const filters: UsageAnalyticsFilters = {
      preset: "7d",
      granularity: "day",
      project_id: "proj-1",
      model: "openai/gpt-4.1",
      agent_type: "worker",
      user_id: "user-1",
    };
    const a = serializeUsageAnalyticsFiltersToSearchParams(filters).toString();
    const b = serializeUsageAnalyticsFiltersToSearchParams(filters).toString();
    expect(a).toBe(b);
  });

  // ── All supported URL keys ────────────────────────────────────────────────

  it("serializes all supported URL keys with preset", () => {
    const result = serializeUsageAnalyticsFiltersToSearchParams({
      preset: "7d",
      granularity: "week",
      project_id: "proj-abc",
      model: "openai/gpt-4.1",
      agent_type: "worker",
      user_id: "user-123",
    });
    expect(result.get("preset")).toBe("7d");
    expect(result.get("granularity")).toBe("week");
    expect(result.get("project_id")).toBe("proj-abc");
    expect(result.get("model")).toBe("openai/gpt-4.1");
    expect(result.get("agent_type")).toBe("worker");
    expect(result.get("user_id")).toBe("user-123");
  });

  it("serializes all supported URL keys with custom dates", () => {
    const result = serializeUsageAnalyticsFiltersToSearchParams({
      start: "2026-01-01",
      end: "2026-03-31",
      granularity: "month",
      project_id: "proj-xyz",
      model: "anthropic/claude-sonnet-4-20250514",
      agent_type: "planner",
      user_id: "user-456",
    });
    expect(result.get("start")).toBe("2026-01-01");
    expect(result.get("end")).toBe("2026-03-31");
    expect(result.get("granularity")).toBe("month");
    expect(result.get("project_id")).toBe("proj-xyz");
    expect(result.get("model")).toBe("anthropic/claude-sonnet-4-20250514");
    expect(result.get("agent_type")).toBe("planner");
    expect(result.get("user_id")).toBe("user-456");
    expect(result.get("preset")).toBeNull();
  });

  // ── Invalid preset in serializer ──────────────────────────────────────────

  it("falls back to start/end when preset value is invalid", () => {
    const result = serializeUsageAnalyticsFiltersToSearchParams({
      preset: "90d" as unknown as UsageAnalyticsFilters["preset"],
      start: "2026-01-01",
      end: "2026-01-31",
    });
    expect(result.get("preset")).toBeNull();
    expect(result.get("start")).toBe("2026-01-01");
    expect(result.get("end")).toBe("2026-01-31");
  });
});

// ── Round-trip ───────────────────────────────────────────────────────────────

describe("parse/serialize round-trip", () => {
  it("round-trips preset-based filters", () => {
    const original: UsageAnalyticsFilters = {
      preset: "7d",
      granularity: "week",
      project_id: "proj-1",
      model: "openai/gpt-4.1",
    };
    const params = serializeUsageAnalyticsFiltersToSearchParams(original);
    const restored = parseUsageAnalyticsFiltersFromSearchParams(params);
    expect(restored).toEqual(original);
  });

  it("round-trips custom-date filters", () => {
    const original: UsageAnalyticsFilters = {
      start: "2026-01-01",
      end: "2026-03-31",
      granularity: "month",
      project_id: "proj-xyz",
      agent_type: "planner",
      user_id: "user-456",
    };
    const params = serializeUsageAnalyticsFiltersToSearchParams(original);
    const restored = parseUsageAnalyticsFiltersFromSearchParams(params);
    // preset should NOT be present in either direction
    expect(restored.preset).toBeUndefined();
    expect(restored.start).toBe(original.start);
    expect(restored.end).toBe(original.end);
    expect(restored.granularity).toBe(original.granularity);
    expect(restored.project_id).toBe(original.project_id);
    expect(restored.agent_type).toBe(original.agent_type);
    expect(restored.user_id).toBe(original.user_id);
  });
});

// ── DEFAULT_USAGE_FILTERS ───────────────────────────────────────────────────

describe("DEFAULT_USAGE_FILTERS", () => {
  it("matches the expected dashboard defaults", () => {
    expect(DEFAULT_USAGE_FILTERS).toEqual({
      preset: "30d",
      granularity: "day",
    });
  });
});
