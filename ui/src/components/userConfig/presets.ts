import type { UserModel } from "@/api/userConfig";
import { type ModelLanes } from "@/api/userSettings";

/**
 * Working-style presets for the Model Roles tab (slice 3 of p8py). Exactly two,
 * plus the cross-model review toggle — there is deliberately NO "Max savings".
 *
 * A preset is a convenience that seeds the three lanes (plan / implement /
 * review) from the user's connected+allowed models. The lanes remain the
 * persisted source of truth and stay user-editable underneath; picking a preset
 * just overwrites them with sensible defaults.
 *
 * - `balanced`   — a smart model up top in Plan, cheaper models in
 *                  Implement/Review. The everyday default.
 * - `maxQuality` — the best model in every lane. Also forces cross-model review
 *                  ON when ≥2 distinct model ids exist (the dispatch falls back
 *                  to same-model in the single-model degenerate case).
 */
export type PresetKey = "balanced" | "maxQuality";

export const PRESETS: { key: PresetKey; title: string; description: string }[] = [
  {
    key: "balanced",
    title: "Balanced",
    description: "Smart model for planning, cheaper models to build and review.",
  },
  {
    key: "maxQuality",
    title: "Max quality",
    description: "Best model everywhere, with cross-model review when possible.",
  },
];

/** Combined per-token price of a model (input + output). Higher = pricier. */
function priceOf(model: UserModel): number {
  const p = model.pricing;
  return (p?.input_per_million ?? 0) + (p?.output_per_million ?? 0);
}

/**
 * Rank connected models best → worst as a capability proxy. Reasoning models
 * outrank non-reasoning ones; within each group, pricier outranks cheaper
 * (price is a coarse stand-in for capability when no quality score exists).
 * Ties break on id for stable, deterministic output.
 */
export function rankByQuality(models: UserModel[]): UserModel[] {
  return [...models].sort((a, b) => {
    const reasoning = Number(b.reasoning) - Number(a.reasoning);
    if (reasoning !== 0) return reasoning;
    const price = priceOf(b) - priceOf(a);
    if (price !== 0) return price;
    return a.id.localeCompare(b.id);
  });
}

/** Rank connected models cheapest → priciest (ties break on id). */
export function rankByCost(models: UserModel[]): UserModel[] {
  return [...models].sort((a, b) => {
    const price = priceOf(a) - priceOf(b);
    if (price !== 0) return price;
    return a.id.localeCompare(b.id);
  });
}

/**
 * Build the lane selection for a preset from the user's connected models.
 *
 * Lanes are ordered fallback lists, so we seed each lane with the full ranked
 * list (best/cheapest first) — that gives a sensible primary plus automatic
 * fallbacks. Returns all-empty lanes when the user has no connected models.
 *
 * - `balanced`   — Plan ranked by quality; Implement + Review ranked by cost.
 * - `maxQuality` — every lane ranked by quality.
 */
export function lanesForPreset(preset: PresetKey, models: UserModel[]): ModelLanes {
  if (models.length === 0) {
    return { plan: [], implement: [], review: [] };
  }
  const byQuality = rankByQuality(models).map((m) => m.id);
  const byCost = rankByCost(models).map((m) => m.id);

  if (preset === "maxQuality") {
    return { plan: byQuality, implement: byQuality, review: byQuality };
  }
  // balanced
  return { plan: byQuality, implement: byCost, review: byCost };
}

/**
 * Distinct model ids reachable by the Implement + Review lanes. "Distinct" is by
 * model id, NOT provider — one provider can host many models. This is the set
 * the cross-model review gate counts.
 */
export function reviewDiversityModelIds(lanes: ModelLanes): Set<string> {
  const ids = new Set<string>();
  for (const id of lanes.implement) ids.add(id);
  for (const id of lanes.review) ids.add(id);
  return ids;
}

/**
 * Whether cross-model ("Thorough") review can be enabled: it needs ≥2 distinct
 * model ids across the Implement + Review lanes. Below that the reviewer can
 * never pick a model different from the implementer, so the toggle is disabled
 * (and re-enables automatically once a 2nd distinct id appears).
 */
export function canEnableDiverseReview(lanes: ModelLanes): boolean {
  return reviewDiversityModelIds(lanes).size >= 2;
}
