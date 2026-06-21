import { describe, expect, it } from "vitest";

import type { UserModel } from "@/api/userConfig";
import {
  canEnableDiverseReview,
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

const SMART = model("anthropic/opus", {
  reasoning: true,
  pricing: { input_per_million: 15, output_per_million: 75, cache_read_per_million: 0, cache_write_per_million: 0 },
});
const CHEAP = model("openai/mini", {
  reasoning: false,
  pricing: { input_per_million: 1, output_per_million: 4, cache_read_per_million: 0, cache_write_per_million: 0 },
});
const MID = model("fireworks/kimi", {
  reasoning: true,
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

describe("lanesForPreset", () => {
  it("balanced: smart-first plan, cheap-first implement/review", () => {
    const lanes = lanesForPreset("balanced", [SMART, CHEAP, MID]);
    expect(lanes.plan[0]).toBe("anthropic/opus");
    expect(lanes.implement[0]).toBe("openai/mini");
    expect(lanes.review[0]).toBe("openai/mini");
  });

  it("maxQuality: best model first in every lane", () => {
    const lanes = lanesForPreset("maxQuality", [SMART, CHEAP, MID]);
    expect(lanes.plan[0]).toBe("anthropic/opus");
    expect(lanes.implement[0]).toBe("anthropic/opus");
    expect(lanes.review[0]).toBe("anthropic/opus");
  });

  it("returns empty lanes for no connected models", () => {
    expect(lanesForPreset("balanced", [])).toEqual({ plan: [], implement: [], review: [] });
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
