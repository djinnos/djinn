/**
 * Pure URL parse/serialize helpers for the `/admin/usage` dashboard filters.
 *
 * These helpers convert between `UsageAnalyticsFilters` (the in-memory shape
 * consumed by React components and the API query-string builder) and
 * `URLSearchParams` (the browser-native representation of the route's search
 * string).  They are intentionally framework-free — no React Router
 * dependency — so they can be imported from any layer and unit-tested in
 * isolation.
 *
 * Design invariants
 * ─────────────────
 * • Supported URL keys match the API filter fields exactly:
 *   `preset`, `start`, `end`, `granularity`, `project_id`, `model`,
 *   `agent_type`, `user_id`.
 * • Valid `preset` values: `7d`, `30d`.  Valid `granularity` values:
 *   `day`, `week`, `month`.  Invalid enum values are silently dropped.
 * • When no relevant search-params are present the parser returns the
 *   last-30-days daily default: `{ preset: "30d", granularity: "day" }`.
 * • Preset and custom dates are mutually exclusive: a valid `preset` takes
 *   precedence and `start`/`end` are ignored; when no preset is present
 *   `start`/`end` are carried through.
 * • The serializer omits empty strings and `undefined` fields, enforces the
 *   same preset-vs-dates precedence, and returns a `URLSearchParams` without
 *   a leading `?`.
 */

import type { UsageAnalyticsFilters, DateRangePreset, Granularity } from "@/api/analytics";

// ── Constants ───────────────────────────────────────────────────────────────

const VALID_PRESETS: ReadonlySet<DateRangePreset> = new Set(["7d", "30d"]);
const VALID_GRANULARITIES: ReadonlySet<Granularity> = new Set(["day", "week", "month"]);

/** Default dashboard state: last 30 days at daily granularity. */
export const DEFAULT_USAGE_FILTERS: UsageAnalyticsFilters = {
  preset: "30d",
  granularity: "day",
};

// ── Parse ───────────────────────────────────────────────────────────────────

/**
 * Restore a `UsageAnalyticsFilters` from browser URL search-params.
 *
 * Unknown keys are ignored.  Invalid enum values for `preset` or
 * `granularity` are silently dropped (the corresponding field will be
 * `undefined` and the default may apply).
 */
export function parseUsageAnalyticsFiltersFromSearchParams(
  searchParams: URLSearchParams,
): UsageAnalyticsFilters {
  const rawPreset = searchParams.get("preset");
  const rawGranularity = searchParams.get("granularity");
  const rawStart = searchParams.get("start");
  const rawEnd = searchParams.get("end");
  const rawProjectId = searchParams.get("project_id");
  const rawModel = searchParams.get("model");
  const rawAgentType = searchParams.get("agent_type");
  const rawUserId = searchParams.get("user_id");

  // Validate enums — drop silently when invalid.
  const preset: DateRangePreset | undefined =
    rawPreset && VALID_PRESETS.has(rawPreset as DateRangePreset)
      ? (rawPreset as DateRangePreset)
      : undefined;

  const granularity: Granularity | undefined =
    rawGranularity && VALID_GRANULARITIES.has(rawGranularity as Granularity)
      ? (rawGranularity as Granularity)
      : undefined;

  // Preset-vs-custom-dates precedence: when a valid preset is present the
  // start/end dates are intentionally ignored.
  const start = preset ? undefined : nonEmpty(rawStart);
  const end = preset ? undefined : nonEmpty(rawEnd);

  // String filters — pass through when non-empty.
  const project_id = nonEmpty(rawProjectId);
  const model = nonEmpty(rawModel);
  const agent_type = nonEmpty(rawAgentType);
  const user_id = nonEmpty(rawUserId);

  // Build the result, falling back to defaults when nothing relevant was
  // supplied in the URL.
  const filters: UsageAnalyticsFilters = {
    preset: preset ?? DEFAULT_USAGE_FILTERS.preset,
    granularity: granularity ?? DEFAULT_USAGE_FILTERS.granularity,
    ...(start !== undefined && { start }),
    ...(end !== undefined && { end }),
    ...(project_id !== undefined && { project_id }),
    ...(model !== undefined && { model }),
    ...(agent_type !== undefined && { agent_type }),
    ...(user_id !== undefined && { user_id }),
  };

  // When custom dates are used (no preset), remove the preset field entirely
  // so the downstream API query-string builder treats them as mutually
  // exclusive correctly.
  if (start !== undefined || end !== undefined) {
    delete filters.preset;
  }

  return filters;
}

// ── Serialize ───────────────────────────────────────────────────────────────

/**
 * Write a `UsageAnalyticsFilters` into a `URLSearchParams` suitable for
 * inclusion in a route's `search` property (no leading `?`).
 *
 * Empty strings, `undefined` values, and (when a preset is present)
 * `start`/`end` fields are omitted so the URL stays minimal and stable.
 */
export function serializeUsageAnalyticsFiltersToSearchParams(
  filters: UsageAnalyticsFilters,
): URLSearchParams {
  const params = new URLSearchParams();

  // Preset-vs-dates precedence: when preset is set, omit start/end.
  if (filters.preset && VALID_PRESETS.has(filters.preset)) {
    params.set("preset", filters.preset);
  } else {
    setIfNonEmpty(params, "start", filters.start);
    setIfNonEmpty(params, "end", filters.end);
  }

  setIfNonEmpty(params, "granularity", filters.granularity);
  setIfNonEmpty(params, "project_id", filters.project_id);
  setIfNonEmpty(params, "model", filters.model);
  setIfNonEmpty(params, "agent_type", filters.agent_type);
  setIfNonEmpty(params, "user_id", filters.user_id);

  return params;
}

// ── Internal helpers ────────────────────────────────────────────────────────

/** Return the string when it is non-null and non-empty, otherwise `undefined`. */
function nonEmpty(value: string | null): string | undefined {
  return value != null && value.length > 0 ? value : undefined;
}

/** `params.set` only when `value` is a non-empty string. */
function setIfNonEmpty(
  params: URLSearchParams,
  key: string,
  value: string | undefined,
): void {
  if (value != null && value.length > 0) {
    params.set(key, value);
  }
}
