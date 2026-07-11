import { describe, expect, it, vi } from "vitest";

import type { UserModel } from "@/api/userConfig";
import { render, screen } from "@/test/test-utils";

import { ModelSection } from "./ModelSection";
import {
  allModelsSorted,
  formatModelMetadata,
  groupModelsByProvider,
  pickableModels,
  providerDefaultModels,
  sortModels,
  stripProviderPrefix,
} from "./modelPicker";

function um(id: string, opts: Partial<UserModel> = {}): UserModel {
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

vi.mock("@/lib/toast", () => ({
  showToast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

vi.mock("@/api/userConfig", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/userConfig")>();
  return {
    ...actual,
    fetchUserConnectedModels: vi.fn(async () => [
      {
        id: "openai/gpt-5",
        name: "GPT-5",
        provider_id: "openai",
        tool_call: true,
      } as UserModel,
      {
        id: "anthropic/claude-sonnet-4",
        name: "Claude Sonnet 4",
        provider_id: "anthropic",
        tool_call: true,
      } as UserModel,
    ]),
    fetchUserModelSelection: vi.fn(async () => ({
      lanes: {
        plan: ["openai/gpt-5"],
        implement: [],
        review: [],
      },
      maxSessions: { "openai/gpt-5": 3 },
    })),
    saveUserModelSelection: vi.fn(),
  };
});

describe("pickableModels", () => {
  it("offers only recommended flagships when a provider has them", () => {
    const offered = pickableModels([
      um("openai/gpt-5.5", { recommended: true }),
      um("openai/gpt-5.3"),
      um("openai/o3"),
    ]).map((m) => m.id);
    expect(offered).toEqual(["openai/gpt-5.5"]);
  });

  it("falls back to ALL of a provider's models when none are recommended", () => {
    const offered = pickableModels([
      um("local/a"),
      um("local/b"),
    ]).map((m) => m.id).sort();
    expect(offered).toEqual(["local/a", "local/b"]);
  });

  it("mixes per provider: flagship for curated, all for uncurated", () => {
    const offered = new Set(
      pickableModels([
        um("openai/gpt-5.5", { recommended: true }),
        um("openai/gpt-5.3"),
        um("local/x"),
        um("local/y"),
      ]).map((m) => m.id),
    );
    expect(offered.has("openai/gpt-5.5")).toBe(true);
    expect(offered.has("openai/gpt-5.3")).toBe(false);
    expect(offered.has("local/x")).toBe(true);
    expect(offered.has("local/y")).toBe(true);
  });
});

describe("providerDefaultModels", () => {
  it("preserves recommended-only default for providers with recommendations", () => {
    const models = [
      um("openai/gpt-5.5", { recommended: true, name: "GPT-5.5" }),
      um("openai/gpt-5.3", { name: "GPT-5.3" }),
      um("openai/o3", { name: "o3" }),
    ];
    const defaults = providerDefaultModels(models);
    expect(defaults.map((m) => m.id)).toEqual(["openai/gpt-5.5"]);
  });

  it("falls back to ALL models when no provider has recommendations", () => {
    const models = [um("local/a", { name: "A" }), um("local/b", { name: "B" })];
    const defaults = providerDefaultModels(models);
    expect(defaults.map((m) => m.id)).toEqual(["local/a", "local/b"]);
  });

  it("returns recommended for curated provider + all for uncurated in one pass", () => {
    const models = [
      um("openai/gpt-5.5", { recommended: true, name: "GPT-5.5" }),
      um("openai/gpt-5.3", { name: "GPT-5.3" }),
      um("local/x", { name: "X" }),
      um("local/y", { name: "Y" }),
    ];
    const defaults = providerDefaultModels(models);
    const ids = defaults.map((m) => m.id);
    expect(ids).toContain("openai/gpt-5.5");
    expect(ids).not.toContain("openai/gpt-5.3");
    expect(ids).toContain("local/x");
    expect(ids).toContain("local/y");
  });
});

describe("allModelsSorted", () => {
  it("retains every model including non-recommended", () => {
    const models = [
      um("openai/gpt-5.5", { recommended: true, name: "GPT-5.5" }),
      um("openai/gpt-5.3", { name: "GPT-5.3" }),
      um("openai/o3", { name: "o3" }),
      um("local/x", { name: "X" }),
    ];
    const all = allModelsSorted(models);
    expect(all).toHaveLength(4);
    const ids = all.map((m) => m.id);
    expect(ids).toContain("openai/gpt-5.5");
    expect(ids).toContain("openai/gpt-5.3");
    expect(ids).toContain("openai/o3");
    expect(ids).toContain("local/x");
  });

  it("sorts recommended first, then by name", () => {
    const models = [
      um("p/b", { name: "B" }),
      um("p/a", { recommended: true, name: "A" }),
      um("p/c", { name: "C" }),
    ];
    const sorted = allModelsSorted(models);
    expect(sorted[0]!.id).toBe("p/a"); // recommended first
    expect(sorted[1]!.id).toBe("p/b"); // name B < C
    expect(sorted[2]!.id).toBe("p/c");
  });
});

describe("sortModels", () => {
  it("sorts recommended first, then by name (case-insensitive), then by id", () => {
    const models = [
      um("p/z", { recommended: true, name: "Zebra" }),
      um("p/a", { recommended: true, name: "alpha" }),
      um("p/m", { name: "Mango" }),
      um("p/b", { name: "banana" }),
    ];
    const sorted = sortModels(models).map((m) => m.id);
    expect(sorted).toEqual(["p/a", "p/z", "p/b", "p/m"]);
  });

  it("uses full id as tie-breaker when names match", () => {
    const models = [
      um("provider/b-model", { name: "Same" }),
      um("provider/a-model", { name: "Same" }),
    ];
    const sorted = sortModels(models).map((m) => m.id);
    expect(sorted).toEqual(["provider/a-model", "provider/b-model"]);
  });

  it("does not mutate the input array", () => {
    const models = [um("p/b", { name: "B" }), um("p/a", { name: "A" })];
    const original = [...models];
    sortModels(models);
    expect(models.map((m) => m.id)).toEqual(original.map((m) => m.id));
  });
});

describe("groupModelsByProvider", () => {
  it("groups models by provider_id with alphabetical provider ordering", () => {
    const models = [
      um("openai/gpt-5", { name: "GPT-5" }),
      um("anthropic/claude", { name: "Claude" }),
      um("openai/o3", { name: "o3" }),
    ];
    const groups = groupModelsByProvider(models);
    expect(groups).toHaveLength(2);
    expect(groups[0]!.providerId).toBe("anthropic");
    expect(groups[0]!.models.map((m) => m.id)).toEqual(["anthropic/claude"]);
    expect(groups[1]!.providerId).toBe("openai");
    expect(groups[1]!.models.map((m) => m.id)).toEqual(["openai/gpt-5", "openai/o3"]);
  });

  it("sorts recommended first within each provider group", () => {
    const models = [
      um("openai/o3", { name: "o3" }),
      um("openai/gpt-5", { recommended: true, name: "GPT-5" }),
    ];
    const groups = groupModelsByProvider(models);
    expect(groups[0]!.providerId).toBe("openai");
    expect(groups[0]!.models[0]!.id).toBe("openai/gpt-5"); // recommended first
    expect(groups[0]!.models[1]!.id).toBe("openai/o3");
  });

  it("uses 'unknown' for models without provider_id", () => {
    const model = um("custom-model", { provider_id: undefined as any });
    const groups = groupModelsByProvider([model]);
    expect(groups).toHaveLength(1);
    expect(groups[0]!.providerId).toBe("unknown");
  });

  it("produces deterministic output regardless of input order", () => {
    const models = [
      um("zeta/b", { name: "B" }),
      um("alpha/c", { name: "C" }),
      um("alpha/a", { name: "A" }),
      um("zeta/a", { name: "A" }),
    ];
    const g1 = groupModelsByProvider(models);
    const g2 = groupModelsByProvider([...models].reverse());
    expect(g1.map((g) => g.providerId)).toEqual(g2.map((g) => g.providerId));
    expect(g1.flatMap((g) => g.models.map((m) => m.id))).toEqual(
      g2.flatMap((g) => g.models.map((m) => m.id)),
    );
  });
});

describe("multi-segment full id preservation", () => {
  it("preserves multi-segment provider-local ids exactly", () => {
    const fullId = "fireworks/accounts/fireworks/models/mimo-v2.5-pro";
    const model = um(fullId, {
      provider_id: "fireworks",
      name: "MiMo v2.5 Pro",
    });
    const sorted = allModelsSorted([model]);
    expect(sorted[0]!.id).toBe(fullId);
  });

  it("preserves multi-segment ids in groups", () => {
    const fullId = "fireworks/accounts/fireworks/models/mimo-v2.5-pro";
    const model = um(fullId, {
      provider_id: "fireworks",
      name: "MiMo v2.5 Pro",
    });
    const groups = groupModelsByProvider([model]);
    expect(groups[0]!.models[0]!.id).toBe(fullId);
  });

  it("uses full multi-segment id as tie-breaker", () => {
    const m1 = um("fireworks/accounts/z-model", {
      provider_id: "fireworks",
      name: "Same",
    });
    const m2 = um("fireworks/accounts/a-model", {
      provider_id: "fireworks",
      name: "Same",
    });
    const sorted = sortModels([m1, m2]);
    expect(sorted[0]!.id).toBe("fireworks/accounts/a-model");
    expect(sorted[1]!.id).toBe("fireworks/accounts/z-model");
  });
});

describe("formatModelMetadata", () => {
  it("renders context, pricing, and capability chips", () => {
    const meta = formatModelMetadata(
      um("openai/gpt-5", {
        name: "GPT-5",
        context_window: 1_000_000,
        output_limit: 8_000,
        pricing: {
          input_per_million: 2.5,
          output_per_million: 10,
          cache_read_per_million: 0,
          cache_write_per_million: 0,
        },
        reasoning: true,
        tool_call: true,
        attachment: true,
      }),
    );
    expect(meta).toContain("1M ctx");
    expect(meta).toContain("$2.50/$10.00 per M tok");
    expect(meta).toContain("reasoning");
    expect(meta).toContain("tools");
    expect(meta).toContain("vision");
  });

  it("renders only input pricing when output is zero", () => {
    const meta = formatModelMetadata(
      um("openai/gpt-5", {
        tool_call: false,
        pricing: {
          input_per_million: 1.25,
          output_per_million: 0,
          cache_read_per_million: 0,
          cache_write_per_million: 0,
        },
      }),
    );
    expect(meta).toContain("$1.25 in");
    expect(meta).not.toContain("per M tok");
  });

  it("renders only output pricing when input is zero", () => {
    const meta = formatModelMetadata(
      um("openai/gpt-5", {
        tool_call: false,
        pricing: {
          input_per_million: 0,
          output_per_million: 5,
          cache_read_per_million: 0,
          cache_write_per_million: 0,
        },
      }),
    );
    expect(meta).toContain("$5.00 out");
    expect(meta).not.toContain("per M tok");
  });

  it("does not include misleading zeros for missing metadata", () => {
    const meta = formatModelMetadata(
      um("openai/gpt-5", { tool_call: false, context_window: 0, pricing: undefined }),
    );
    expect(meta).toBe("");
  });

  it("handles a model with only reasoning", () => {
    const meta = formatModelMetadata(um("openai/gpt-5", { tool_call: false, reasoning: true }));
    expect(meta).toBe("reasoning");
  });
});

describe("ModelSection", () => {
  it("strips provider prefixes for fallback model display", () => {
    expect(stripProviderPrefix("openai/gpt-5")).toBe("gpt-5");
    expect(stripProviderPrefix("custom-model")).toBe("custom-model");
  });

  it("smoke-renders per-role model lanes", async () => {
    render(<ModelSection targetId="target-user" />);

    expect(screen.getByRole("heading", { name: "Model roles" })).toBeInTheDocument();
    // The model selected for the `plan` lane renders with its provider + cap.
    expect(await screen.findByText("GPT-5")).toBeInTheDocument();
    expect(screen.getByText("OpenAI")).toBeInTheDocument();
    expect(screen.getByDisplayValue("3")).toBeInTheDocument();
    // Each lane exposes its own Add model trigger.
    expect(
      screen.getAllByRole("button", { name: "Add model" }).length,
    ).toBeGreaterThanOrEqual(3);
  });
});
