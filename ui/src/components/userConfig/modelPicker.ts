import type { UserModel } from "@/api/userConfig";

/** Strips the provider prefix from a model id ("openai/gpt-5" → "gpt-5"). */
export function stripProviderPrefix(modelId: string): string {
  const slash = modelId.indexOf("/");
  return slash >= 0 ? modelId.slice(slash + 1) : modelId;
}

/**
 * The curated set of models offered in the "Add model" picker: only each
 * provider's recommended flagship(s) (PR-1's `recommended`), so users stop
 * scrolling past GPT-5.2/5.3/5.4/o1/o3 clutter. A provider with NO curated
 * recommendation falls back to ALL its models, so nothing becomes unselectable.
 * There is intentionally no "show all" escape hatch — the lanes stay clean.
 *
 * (Already-selected non-flagship models still render in their lane via the
 * separate `modelsById` lookup; this only governs what the picker OFFERS.)
 */
export function pickableModels(models: UserModel[]): UserModel[] {
  const byProvider = new Map<string, UserModel[]>();
  for (const model of models) {
    const provider = model.provider_id ?? "unknown";
    if (!byProvider.has(provider)) byProvider.set(provider, []);
    byProvider.get(provider)!.push(model);
  }
  const pickable: UserModel[] = [];
  for (const providerModels of byProvider.values()) {
    const recommended = providerModels.filter((m) => m.recommended);
    pickable.push(...(recommended.length > 0 ? recommended : providerModels));
  }
  return pickable;
}
