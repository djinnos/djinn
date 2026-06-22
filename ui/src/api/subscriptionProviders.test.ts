import { describe, expect, it } from "vitest";

import { isSubscriptionProvider } from "./subscriptionProviders";

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
