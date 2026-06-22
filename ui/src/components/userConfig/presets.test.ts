import { describe, expect, it } from "vitest";

import type { UserModel } from "@/api/userConfig";
import {
  canEnableDiverseReview,
  flagshipsPerProvider,
  lanesForPreset,
  rankByCost,
  rankByQuality,
  reviewDiversityModelIds,
} from "./presets";

function model(id: string, opts: Partial<UserModel> = {}): UserModel {
  return {
    id,
    name: id,
    provider_id: id.split("/")[0] ?? "p",
    attachment: false,
    context_window: 0,
    output_limit: 0,
    reasoning: false,
    recommended: false,
    tool_call: true,
    pricing: {
      input_per_million: 0,
      output_per_million: 0,
      cache_read_per_million: 0,
      cache_write_per_million: 0,
    },
    ...opts,
  } as UserModel;
}

function priced(
  id: string,
  input: number,
  output: number,
  opts: Partial<UserModel> = {},
): UserModel {
  return model(id, {
    pricing: {
      input_per_million: input,
      output_per_million: output,
      cache_read_per_million: 0,
      cache_write_per_million: 0,
    },
    ...opts,
  });
}

const SMART = model("anthropic/opus", {
  reasoning: true,
  recommended: true,
  pricing: { input_per_million: 15, output_per_million: 75, cache_read_per_million: 0, cache_write_per_million: 0 },
});
const CHEAP = model("openai/mini", {
  reasoning: false,
  recommended: true,
  pricing: { input_per_million: 1, output_per_million: 4, cache_read_per_million: 0, cache_write_per_million: 0 },
});
const MID = model("fireworks/kimi", {
  reasoning: true,
  recommended: true,
  pricing: { input_per_million: 3, output_per_million: 9, cache_read_per_million: 0, cache_write_per_million: 0 },
});

describe("rankByQuality / rankByCost", () => {
  it("ranks reasoning models first, then by descending price", () => {
    const ranked = rankByQuality([CHEAP, SMART, MID]).map((m) => m.id);
    // SMART + MID reason; SMART pricier → first. CHEAP (no reasoning) last.
    expect(ranked).toEqual(["anthropic/opus", "fireworks/kimi", "openai/mini"]);
  });

  it("ranks cheapest first regardless of reasoning", () => {
    const ranked = rankByCost([SMART, CHEAP, MID]).map((m) => m.id);
    expect(ranked).toEqual(["openai/mini", "fireworks/kimi", "anthropic/opus"]);
  });
});

describe("flagshipsPerProvider", () => {
  it("keeps only the recommended flagship, one per provider", () => {
    const gpt55 = priced("openai/gpt-5.5", 5, 20, { recommended: true });
    const gpt53 = priced("openai/gpt-5.3", 4, 16, { recommended: false });
    const o3 = priced("openai/o3", 10, 40, { recommended: false });
    const flagships = flagshipsPerProvider([gpt53, o3, gpt55]).map((m) => m.id);
    expect(flagships).toEqual(["openai/gpt-5.5"]);
  });

  it("collapses to one representative even with multiple recommended (best by quality)", () => {
    const a = priced("p/a", 1, 1, { recommended: true, reasoning: true });
    const b = priced("p/b", 5, 5, { recommended: true, reasoning: true });
    const flagships = flagshipsPerProvider([a, b]).map((m) => m.id);
    // Both recommended + reasoning; pricier wins as quality proxy.
    expect(flagships).toEqual(["p/b"]);
  });

  it("falls back to a provider's models when it has no recommendation", () => {
    const x = priced("local/x", 0, 0, { recommended: false });
    const y = priced("local/y", 2, 2, { recommended: false, reasoning: true });
    const flagships = flagshipsPerProvider([x, y]).map((m) => m.id);
    // No recommended → fall back, best-by-quality representative.
    expect(flagships).toEqual(["local/y"]);
  });

  it("yields one flagship per provider across a mixed set", () => {
    const gpt55 = priced("openai/gpt-5.5", 5, 20, { recommended: true });
    const gpt53 = priced("openai/gpt-5.3", 4, 16);
    const minimax = priced("minimax/m3", 0.2, 1.1, { recommended: true });
    const flagships = flagshipsPerProvider([gpt53, gpt55, minimax]).map((m) => m.id);
    expect(flagships.sort()).toEqual(["minimax/m3", "openai/gpt-5.5"]);
  });
});

describe("lanesForPreset", () => {
  it("balanced: smart-first plan, cheap-first implement (flagships only)", () => {
    const lanes = lanesForPreset("balanced", [SMART, CHEAP, MID]);
    expect(lanes.plan[0]).toBe("anthropic/opus"); // priciest flagship leads Plan
    expect(lanes.implement[0]).toBe("openai/mini"); // cheapest flagship leads Implement
  });

  it("maxQuality: best flagship first in every lane", () => {
    const lanes = lanesForPreset("maxQuality", [SMART, CHEAP, MID]);
    expect(lanes.plan[0]).toBe("anthropic/opus");
    expect(lanes.implement[0]).toBe("anthropic/opus");
    expect(lanes.review[0]).toBe("anthropic/opus");
  });

  it("returns empty lanes for no connected models", () => {
    expect(lanesForPreset("balanced", [])).toEqual({ plan: [], implement: [], review: [] });
  });

  it("balanced collapses provider variants to flagships before tiering", () => {
    // The classic test user: one expensive US flagship + several cheap subs,
    // each provider carrying extra non-flagship variants that must be dropped.
    const gpt55 = priced("openai/gpt-5.5", 5, 20, { recommended: true, reasoning: true });
    const gpt53 = priced("openai/gpt-5.3", 4, 16); // variant, not recommended
    const o3 = priced("openai/o3", 10, 40); // variant, not recommended
    const minimax = priced("minimax/m3", 0.2, 1.1, { recommended: true });
    const kimi = priced("kimi/k2p7", 0.3, 1.2, { recommended: true });
    const glm = priced("zai/glm-5.2", 0.25, 1.0, { recommended: true });
    const mimo = priced("xiaomi/mimo-v2.5-pro", 0.15, 0.9, { recommended: true });

    const lanes = lanesForPreset("balanced", [
      gpt55, gpt53, o3, minimax, kimi, glm, mimo,
    ]);

    // Exactly one flagship per provider reaches the lanes — no GPT-5.3 / o3.
    const all = new Set([...lanes.plan, ...lanes.implement, ...lanes.review]);
    expect(all.has("openai/gpt-5.3")).toBe(false);
    expect(all.has("openai/o3")).toBe(false);
    expect(all.size).toBe(5); // 5 flagships total

    // GPT-5.5 (priciest) leads Plan.
    expect(lanes.plan[0]).toBe("openai/gpt-5.5");
    // A cheap Chinese flagship leads Implement (mimo is cheapest here).
    expect(lanes.implement[0]).toBe("xiaomi/mimo-v2.5-pro");
    // Review leads with a DIFFERENT model than Implement → Thorough reachable.
    expect(lanes.review[0]).not.toBe(lanes.implement[0]);
    expect(canEnableDiverseReview(lanes)).toBe(true);
  });

  it("single-provider user still works (no crash, gate stays closed if 1 flagship)", () => {
    const only = priced("openai/gpt-5.5", 5, 20, { recommended: true });
    const variant = priced("openai/gpt-5.3", 4, 16);
    const lanes = lanesForPreset("balanced", [only, variant]);
    expect(lanes.plan).toEqual(["openai/gpt-5.5"]);
    expect(lanes.implement).toEqual(["openai/gpt-5.5"]);
    expect(lanes.review).toEqual(["openai/gpt-5.5"]); // length 1 → no rotation
    expect(canEnableDiverseReview(lanes)).toBe(false);
  });

  it("providers with no recommendation still seed lanes (not left empty)", () => {
    const a = priced("local/a", 1, 1);
    const b = priced("other/b", 2, 2);
    const lanes = lanesForPreset("balanced", [a, b]);
    expect(lanes.plan.length).toBe(2);
    expect(new Set([...lanes.plan])).toEqual(new Set(["local/a", "other/b"]));
  });
});

describe("cross-model review gate", () => {
  it("counts distinct model ids across implement + review", () => {
    const ids = reviewDiversityModelIds({
      plan: ["x/1"],
      implement: ["a/1"],
      review: ["a/1", "b/2"],
    });
    expect([...ids].sort()).toEqual(["a/1", "b/2"]);
  });

  it("requires ≥2 distinct ids to enable", () => {
    expect(canEnableDiverseReview({ plan: [], implement: ["a/1"], review: ["a/1"] })).toBe(false);
    expect(canEnableDiverseReview({ plan: [], implement: ["a/1"], review: ["b/2"] })).toBe(true);
    expect(canEnableDiverseReview({ plan: [], implement: ["a/1", "b/2"], review: [] })).toBe(true);
  });

  it("distinct is by model id, not provider (one provider, many models)", () => {
    // Both ids are Fireworks-hosted but different models → counts as 2 distinct.
    expect(
      canEnableDiverseReview({ plan: [], implement: ["fireworks/kimi"], review: ["fireworks/qwen"] }),
    ).toBe(true);
  });

  it("plan-lane models do NOT count toward the review gate", () => {
    expect(canEnableDiverseReview({ plan: ["a/1", "b/2"], implement: ["a/1"], review: ["a/1"] })).toBe(
      false,
    );
  });
});
