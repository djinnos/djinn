import type { UserModel } from "@/api/userConfig";

/** Strips the provider prefix from a model id ("openai/gpt-5" → "gpt-5"). */
export function stripProviderPrefix(modelId: string): string {
  const slash = modelId.indexOf("/");
  return slash >= 0 ? modelId.slice(slash + 1) : modelId;
}

/**
 * Compare two models for deterministic ordering:
 *  1. recommended models first
 *  2. then by display name (case-insensitive, falling back to id)
 *  3. then by full id as a tie-breaker (preserves multi-segment ids like
 *     "fireworks/accounts/fireworks/models/mimo-v2.5-pro")
 */
export function compareModels(a: UserModel, b: UserModel): number {
  // Recommended first
  if (a.recommended !== b.recommended) return a.recommended ? -1 : 1;
  // By name (case-insensitive); fall back to id when name is empty
  const aName = (a.name || a.id).toLowerCase();
  const bName = (b.name || b.id).toLowerCase();
  const nameCmp = aName.localeCompare(bName);
  if (nameCmp !== 0) return nameCmp;
  // Tie-breaker by full id
  return a.id.localeCompare(b.id);
}

/**
 * Return a new array sorted: recommended first, then by display name, then by
 * full id. Does not mutate the input.
 */
export function sortModels(models: UserModel[]): UserModel[] {
  return [...models].sort(compareModels);
}

export interface ProviderGroup {
  /** `provider_id ?? "unknown"` — the grouping key and display identifier. */
  providerId: string;
  /** Models belonging to this provider, sorted recommended-first → name → id. */
  models: UserModel[];
}

/**
 * Group models by `provider_id` (falling back to `"unknown"` when absent).
 * Providers are sorted alphabetically by id; models within each group use the
 * recommended-first → name → id sort.
 *
 * The UI task can iterate these groups to render per-provider sections with a
 * consistent, deterministic layout.
 */
export function groupModelsByProvider(models: UserModel[]): ProviderGroup[] {
  const byProvider = new Map<string, UserModel[]>();
  for (const model of models) {
    const provider = model.provider_id ?? "unknown";
    let bucket = byProvider.get(provider);
    if (!bucket) {
      bucket = [];
      byProvider.set(provider, bucket);
    }
    bucket.push(model);
  }

  const groups: ProviderGroup[] = [];
  // Sort provider ids alphabetically for deterministic ordering
  const providerIds = [...byProvider.keys()].sort();
  for (const providerId of providerIds) {
    groups.push({
      providerId,
      models: sortModels(byProvider.get(providerId)!),
    });
  }
  return groups;
}

/**
 * Format a numeric value as millions of tokens with one decimal when useful.
 * Returns undefined for zero/null/undefined/negative values so the caller can
 * decide whether to render.
 */
function formatTokens(value: number | null | undefined): string | undefined {
  if (value == null || value <= 0) return undefined;
  if (value >= 1_000_000) {
    const millions = value / 1_000_000;
    const rounded = Math.round(millions * 10) / 10;
    return `${rounded}M`;
  }
  return `${value}`;
}

function formatPrice(value: number | null | undefined): string | undefined {
  if (value == null || value <= 0) return undefined;
  if (value >= 100) return `$${Math.round(value)}`;
  if (value >= 0.01) return `$${value.toFixed(2)}`;
  return `$${value.toPrecision(2)}`;
}

export function formatModelMetadata(model: UserModel): string {
  const parts: string[] = [];
  const context = formatTokens(model.context_window);
  if (context) parts.push(`${context} ctx`);
  const pricing = model.pricing;
  if (pricing) {
    const inputPrice = formatPrice(pricing.input_per_million);
    const outputPrice = formatPrice(pricing.output_per_million);
    if (inputPrice && outputPrice) {
      parts.push(`${inputPrice}/${outputPrice} per M tok`);
    } else if (inputPrice) {
      parts.push(`${inputPrice} in`);
    } else if (outputPrice) {
      parts.push(`${outputPrice} out`);
    }
  }
  const chips: string[] = [];
  if (model.reasoning) chips.push("reasoning");
  if (model.tool_call) chips.push("tools");
  if (model.attachment) chips.push("vision");
  if (chips.length > 0) parts.push(chips.join(" "));
  return parts.join(" · ");
}

/**
 * Per-provider "default" list: recommended-only when the provider has any
 * recommendation, otherwise all of the provider's models. This is the set the
 * collapsed/default picker UI should display (preserves the original
 * `pickableModels` semantics with consistent sorting).
 *
 * Use `allModelsSorted` for the expanded/browse/search state where every
 * connected model should be reachable.
 */
export function providerDefaultModels(models: UserModel[]): UserModel[] {
  const groups = groupModelsByProvider(models);
  const defaults: UserModel[] = [];
  for (const group of groups) {
    const recommended = group.models.filter((m) => m.recommended);
    defaults.push(...(recommended.length > 0 ? recommended : group.models));
  }
  return defaults;
}

/**
 * Every connected model in recommended-first → name → id order — nothing is
 * dropped. This is the full list for browse/search states where users need to
 * reach any model.
 */
export function allModelsSorted(models: UserModel[]): UserModel[] {
  return sortModels(models);
}

/**
 * The curated set of models offered in the "Add model" picker: only each
 * provider's recommended flagship(s), so users stop scrolling past clutter. A
 * provider with NO curated recommendation falls back to ALL its models, so
 * nothing becomes unselectable.
 *
 * @deprecated Use `providerDefaultModels` for the same semantics with
 * consistent sorting, or `allModelsSorted` / `groupModelsByProvider` for the
 * browse/search surface. Kept for backward compatibility during migration.
 */
export function pickableModels(models: UserModel[]): UserModel[] {
  return providerDefaultModels(models);
}
