import { callMcpTool } from "@/api/mcpClient";

/**
 * Per-user, per-ROLE ordered model selection ("lanes"). Each lane is an ordered
 * fallback list (priority high → low) of full `"provider/model"` ids. A task's
 * base role maps to one lane: `plan` (planner/architect/chat), `implement`
 * (worker), `review` (reviewer).
 */
export interface ModelLanes {
  plan: string[];
  implement: string[];
  review: string[];
}

/** Per-user concurrency ceilings for each role lane. */
export interface LaneMaxSessions {
  plan: number;
  implement: number;
  review: number;
}

/** The three lane keys, in display order. */
export const MODEL_LANE_KEYS = ["plan", "implement", "review"] as const;
export type ModelLaneKey = (typeof MODEL_LANE_KEYS)[number];

export function emptyLanes(): ModelLanes {
  return { plan: [], implement: [], review: [] };
}

/** Conservative defaults used when a user explicitly configures lane limits. */
export function defaultLaneMaxSessions(): LaneMaxSessions {
  return { plan: 1, implement: 1, review: 1 };
}

/**
 * Distinct union of all model ids across every lane (order: plan, implement,
 * review; duplicates dropped). Used where a single flat per-user selection is
 * still meaningful — e.g. the chat model picker and the onboarding gate.
 */
export function lanesUnion(lanes: ModelLanes): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const key of MODEL_LANE_KEYS) {
    for (const id of lanes[key]) {
      if (!seen.has(id)) {
        seen.add(id);
        out.push(id);
      }
    }
  }
  return out;
}

/**
 * Minimum per-model cap needed for all configured lane ceilings to be usable
 * simultaneously. A model selected in multiple lanes needs the sum because
 * its model cap is shared across those lanes.
 */
export function aggregateModelCapacity(
  lanes: ModelLanes,
  laneMaxSessions: LaneMaxSessions,
): Record<string, number> {
  const aggregate: Record<string, number> = {};
  for (const lane of MODEL_LANE_KEYS) {
    for (const modelId of new Set(lanes[lane])) {
      aggregate[modelId] = (aggregate[modelId] ?? 0) + laneMaxSessions[lane];
    }
  }
  return aggregate;
}

/** Normalise a raw, possibly-partial lanes object into a complete `ModelLanes`. */
export function parseLanes(raw: Partial<ModelLanes> | null | undefined): ModelLanes {
  return {
    plan: Array.isArray(raw?.plan) ? raw.plan : [],
    implement: Array.isArray(raw?.implement) ? raw.implement : [],
    review: Array.isArray(raw?.review) ? raw.review : [],
  };
}

/**
 * Parse an optional wire-level lane limit without erasing the legacy/unset
 * distinction. The server returns all three fields together; malformed or
 * partial payloads are treated as unset instead of inventing missing limits.
 */
export function parseLaneMaxSessions(raw: unknown): LaneMaxSessions | undefined {
  if (!raw || typeof raw !== "object") return undefined;
  const candidate = raw as Partial<Record<ModelLaneKey, unknown>>;
  const values = MODEL_LANE_KEYS.map((lane) => candidate[lane]);
  if (
    values.some(
      (value) =>
        !Number.isInteger(value) ||
        (value as number) < 1 ||
        (value as number) > 10,
    )
  ) {
    return undefined;
  }
  return {
    plan: candidate.plan as number,
    implement: candidate.implement as number,
    review: candidate.review as number,
  };
}

export interface UserSettings {
  autoApprovePrs: boolean;
  /** Per-role model lanes (priority high → low per lane). */
  lanes: ModelLanes;
  /**
   * Per-user per-model concurrency caps, keyed by full `"provider/model"` id.
   * `{}` when unset; consumers default missing entries to 1.
   */
  maxSessions: Record<string, number>;
  /** Per-lane concurrency ceilings; undefined preserves legacy/unbounded users. */
  laneMaxSessions?: LaneMaxSessions;
  /**
   * Cross-model ("Thorough") review. When true (the default), the reviewer
   * prefers a model id different from the implementer's. A degenerate
   * single-model selection falls back to same-model review.
   */
  diverseReview: boolean;
  /**
   * Cross-model ("Diverse") refinement. When true (the default), the
   * proposal-refinement roles (advocate, adversary, judge) prefer a model id
   * different from the primary task model. Falls back to same-model when
   * alternatives are unavailable.
   */
  diverseRefinement: boolean;
}

interface RawGet {
  ok?: boolean;
  user_id?: string | null;
  auto_approve_prs?: boolean;
  lanes?: Partial<ModelLanes> | null;
  max_sessions?: Record<string, number> | null;
  lane_max_sessions?: LaneMaxSessions | null;
  diverse_review?: boolean | null;
  diverse_refinement?: boolean | null;
  error?: string | null;
}

interface RawSet {
  ok?: boolean;
  applied?: boolean;
  auto_approve_prs?: boolean | null;
  lanes?: Partial<ModelLanes> | null;
  max_sessions?: Record<string, number> | null;
  lane_max_sessions?: LaneMaxSessions | null;
  diverse_review?: boolean | null;
  diverse_refinement?: boolean | null;
  error?: string | null;
}

function parseMaxSessions(raw: Record<string, number> | null | undefined): Record<string, number> {
  return raw && typeof raw === "object" ? raw : {};
}

export async function fetchUserSettings(): Promise<UserSettings> {
  const resp = (await callMcpTool("user_settings_get", {})) as RawGet;
  if (resp?.ok === false) {
    throw new Error(resp.error ?? "failed to load user settings");
  }
  return {
    autoApprovePrs: Boolean(resp?.auto_approve_prs),
    lanes: parseLanes(resp?.lanes),
    maxSessions: parseMaxSessions(resp?.max_sessions),
    laneMaxSessions: parseLaneMaxSessions(resp?.lane_max_sessions),
    // Cross-model review defaults ON; only an explicit `false` disables it.
    diverseReview: resp?.diverse_review !== false,
    // Cross-model refinement defaults ON; only an explicit `false` disables it.
    diverseRefinement: resp?.diverse_refinement !== false,
  };
}

export async function patchUserSettings(patch: {
  autoApprovePrs?: boolean;
  /** Per-role model lanes (priority order per lane). Omit to keep current. */
  lanes?: ModelLanes;
  /** Per-model caps keyed by full `"provider/model"` id. Omit to keep current. */
  maxSessions?: Record<string, number>;
  /** Per-role concurrency ceilings. Omit to keep current/unset. */
  laneMaxSessions?: LaneMaxSessions;
  /** Cross-model ("Thorough") review toggle. Omit to keep current. */
  diverseReview?: boolean;
  /** Cross-model ("Diverse") refinement toggle. Omit to keep current. */
  diverseRefinement?: boolean;
}): Promise<UserSettings> {
  const args: Record<string, unknown> = {};
  if (patch.autoApprovePrs !== undefined) {
    args.auto_approve_prs = patch.autoApprovePrs;
  }
  if (patch.lanes !== undefined) {
    args.lanes = patch.lanes;
  }
  if (patch.maxSessions !== undefined) {
    args.max_sessions = patch.maxSessions;
  }
  if (patch.laneMaxSessions !== undefined) {
    args.lane_max_sessions = patch.laneMaxSessions;
  }
  if (patch.diverseReview !== undefined) {
    args.diverse_review = patch.diverseReview;
  }
  if (patch.diverseRefinement !== undefined) {
    args.diverse_refinement = patch.diverseRefinement;
  }
  const resp = (await callMcpTool("user_settings_set", args)) as RawSet;
  if (resp?.ok === false) {
    throw new Error(resp.error ?? "failed to save user settings");
  }
  return {
    autoApprovePrs: Boolean(resp?.auto_approve_prs),
    lanes: parseLanes(resp?.lanes),
    maxSessions: parseMaxSessions(resp?.max_sessions),
    laneMaxSessions: parseLaneMaxSessions(resp?.lane_max_sessions),
    diverseReview: resp?.diverse_review !== false,
    diverseRefinement: resp?.diverse_refinement !== false,
  };
}
