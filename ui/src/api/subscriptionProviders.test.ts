import { describe, expect, it } from "vitest";

import {
  groupSubscriptionsByAccount,
  isSubscriptionProvider,
  type GroupableProvider,
} from "./subscriptionProviders";

describe("isSubscriptionProvider", () => {
  it("classifies known personal subscriptions as subscriptions", () => {
    for (const id of [
      "chatgpt_codex",
      "kimi-for-coding",
      "minimax-coding-plan",
      "zai-coding-plan",
      "zhipuai-coding-plan",
      "opencode",
      "xiaomi-token-plan-sgp",
      "xiaomi-token-plan-cn",
      "moonshotai-coding",
      "alibaba-qwen-plan",
    ]) {
      expect(isSubscriptionProvider(id)).toBe(true);
    }
  });

  it("is case-insensitive", () => {
    expect(isSubscriptionProvider("Kimi-For-Coding")).toBe(true);
  });

  it("classifies fungible API-key providers as NOT subscriptions", () => {
    for (const id of [
      "anthropic",
      "openai",
      "google",
      "azure",
      "aws",
      "vertex",
      "fireworks-ai",
      "mistral",
      "groq",
      "some-random-openai-compat",
    ]) {
      expect(isSubscriptionProvider(id)).toBe(false);
    }
  });
});

describe("groupSubscriptionsByAccount", () => {
  const p = (id: string, name: string, env: string): GroupableProvider => ({
    id,
    name,
    env_vars: [env],
  });

  it("collapses providers sharing a primary env var into one account card", () => {
    const groups = groupSubscriptionsByAccount([
      p("xiaomi", "Xiaomi", "XIAOMI_API_KEY"),
      p("xiaomi-token-plan-cn", "Xiaomi Token Plan (China)", "XIAOMI_API_KEY"),
      p("xiaomi-token-plan-ams", "Xiaomi Token Plan (Europe)", "XIAOMI_API_KEY"),
      p("xiaomi-token-plan-sgp", "Xiaomi Token Plan (Singapore)", "XIAOMI_API_KEY"),
      p("zai", "Z.AI", "ZHIPU_API_KEY"),
      p("zai-coding-plan", "Z.AI Coding Plan", "ZHIPU_API_KEY"),
      p("zhipuai", "Zhipu AI", "ZHIPU_API_KEY"),
      p("zhipuai-coding-plan", "Zhipu AI Coding Plan", "ZHIPU_API_KEY"),
      p("opencode", "OpenCode Zen", "OPENCODE_API_KEY"),
      p("opencode-go", "OpenCode Go", "OPENCODE_API_KEY"),
      p("kimi-for-coding", "Kimi For Coding", "KIMI_API_KEY"),
      p("minimax-coding-plan", "MiniMax Token Plan", "MINIMAX_API_KEY"),
    ]);

    const byKey = new Map(groups.map((g) => [g.key, g]));

    // Shared-key accounts collapse, with a sensible single display name.
    expect(byKey.get("XIAOMI_API_KEY")?.name).toBe("Xiaomi");
    expect(byKey.get("XIAOMI_API_KEY")?.providerIds).toEqual([
      "xiaomi",
      "xiaomi-token-plan-cn",
      "xiaomi-token-plan-ams",
      "xiaomi-token-plan-sgp",
    ]);

    // Z.AI + Zhipu (same ZHIPU_API_KEY) become one card, not four rows.
    expect(byKey.get("ZHIPU_API_KEY")?.name).toBe("Z.AI / Zhipu");
    expect(byKey.get("ZHIPU_API_KEY")?.providerIds).toEqual([
      "zai",
      "zai-coding-plan",
      "zhipuai",
      "zhipuai-coding-plan",
    ]);

    expect(byKey.get("OPENCODE_API_KEY")?.name).toBe("OpenCode");
    expect(byKey.get("OPENCODE_API_KEY")?.providerIds).toEqual([
      "opencode",
      "opencode-go",
    ]);

    // Their own keys → standalone cards using the provider's own name.
    expect(byKey.get("KIMI_API_KEY")?.name).toBe("Kimi For Coding");
    expect(byKey.get("KIMI_API_KEY")?.providerIds).toEqual(["kimi-for-coding"]);
    expect(byKey.get("MINIMAX_API_KEY")?.providerIds).toEqual([
      "minimax-coding-plan",
    ]);

    // Exactly five account cards from twelve connected provider ids.
    expect(groups).toHaveLength(5);
  });

  it("falls back to the provider id when a provider has no env vars", () => {
    const groups = groupSubscriptionsByAccount([
      { id: "weird", name: "Weird", env_vars: [] },
    ]);
    expect(groups).toEqual([
      { key: "weird", name: "Weird", providerIds: ["weird"] },
    ]);
  });
});
