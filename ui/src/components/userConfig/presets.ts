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
 * Presets operate on the curated **flagship** models only — the per-provider
 * recommended state-of-the-art model (PR-1's `recommended` field), collapsed to
 * one model per provider. This stops a preset from dumping every GPT-5.x / o-
 * series / quant variant into the lanes: a user with GPT-5.5 plus a few cheap
 * Chinese subscriptions should see GPT-5.5 on Plan and the cheap flagships on
 * Implement/Review — not 30 models.
 *
 * - `balanced`   — the priciest flagship up top in Plan (the "smart" model),
 *                  the cheap flagships in Implement/Review. The everyday default.
 * - `maxQuality` — the best flagship(s) by quality in every lane. Also forces
 *                  cross-model review ON when ≥2 distinct model ids exist (the
 *                  dispatch falls back to same-model in the single-model case).
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
 * Collapse the connected models to **one curated flagship per provider** — the
 * representative model each preset reasons over.
 *
 * For a provider that HAS curated flagship(s) (`recommended: true`), we keep the
 * single best-by-quality recommended model as its representative. For a provider
 * with NO recommendation (every model `recommended: false` — e.g. a custom or
 * uncurated provider), we fall back to its best-by-quality model so the user is
 * never left with an empty lane just because their provider isn't curated.
 *
 * The result is deterministic and ordered by provider id for stable output.
 */
export function flagshipsPerProvider(models: UserModel[]): UserModel[] {
  const byProvider = new Map<string, UserModel[]>();
  for (const model of models) {
    const provider = model.provider_id ?? "unknown";
    if (!byProvider.has(provider)) byProvider.set(provider, []);
    byProvider.get(provider)!.push(model);
  }

  const reps: UserModel[] = [];
  for (const [, providerModels] of [...byProvider.entries()].sort(([a], [b]) =>
    a.localeCompare(b),
  )) {
    const recommended = providerModels.filter((m) => m.recommended);
    // Prefer the curated flagship(s); fall back to all models when a provider
    // has no recommendation so it still contributes a representative.
    const pool = recommended.length > 0 ? recommended : providerModels;
    const [best] = rankByQuality(pool);
    if (best) reps.push(best);
  }
  return reps;
}

/**
 * Build the lane selection for a preset from the user's connected models.
 *
 * Lanes are ordered fallback lists. Presets seed each lane from the curated
 * per-provider flagships (best/cheapest first) — a sensible primary plus
 * automatic fallbacks, without the variant clutter. Returns all-empty lanes
 * when the user has no connected models.
 *
 * - `balanced`   — Plan = flagships priciest-first (the smart model leads);
 *                  Implement = flagships cheapest-first; Review = the same cheap
 *                  flagships but rotated so its FIRST entry differs from
 *                  Implement's (so Thorough review reaches for a 2nd model).
 * - `maxQuality` — every lane = flagships ranked by quality (best first).
 */
export function lanesForPreset(preset: PresetKey, models: UserModel[]): ModelLanes {
  if (models.length === 0) {
    return { plan: [], implement: [], review: [] };
  }

  const flagships = flagshipsPerProvider(models);
  const byQuality = rankByQuality(flagships).map((m) => m.id);

  if (preset === "maxQuality") {
    return { plan: byQuality, implement: byQuality, review: byQuality };
  }

  // balanced
  const byCost = rankByCost(flagships);
  const cheapIds = byCost.map((m) => m.id);
  // Plan leads with the most EXPENSIVE flagship (the "smart" model), the rest
  // trailing as fallback.
  const byPrice = [...byCost].reverse().map((m) => m.id);
  // Review uses the same cheap pool, but rotated by one so its first entry is a
  // different model than Implement's first — that gives Thorough review a
  // genuinely distinct primary reviewer. With a single flagship there's nothing
  // to rotate, so it degenerates to the same single id (gate stays closed).
  const reviewIds = rotateFirst(cheapIds);

  return { plan: byPrice, implement: cheapIds, review: reviewIds };
}

/**
 * Move the first element to the end, so `[a, b, c]` → `[b, c, a]`. Used to give
 * the Review lane a different lead model than Implement. Arrays of length < 2
 * are returned unchanged (nothing distinct to rotate to).
 */
function rotateFirst(ids: string[]): string[] {
  if (ids.length < 2) return ids;
  return [...ids.slice(1), ids[0]];
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
