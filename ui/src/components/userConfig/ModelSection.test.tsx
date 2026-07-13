import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  fetchUserConnectedModels,
  fetchUserModelSelection,
  saveUserModelSelection,
  type UserModel,
} from "@/api/userConfig";
import { render, screen, userEvent, waitFor, within } from "@/test/test-utils";

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

/**
 * Rich fixture set covering the browse/search picker edge cases mandated by the
 * task design:
 * - `acme` provider with recommended `Zeta` whose name sorts AFTER the
 *   non-recommended `Alpha` alphabetically — catches alphabetical-only sort.
 *   Also includes `acme/legacy.v2.5`, a dotted non-recommended id, to guard
 *   id round-tripping without splitting on `.`.
 * - `beta` provider with no recommendations → all models visible by fallback.
 * - `fireworks` provider with a multi-segment id + full metadata.
 */
function pickerFixtures(): UserModel[] {
  return [
    // acme: recommended Zeta (name "Zeta") sorts AFTER non-recommended Alpha.
    um("acme/alpha", { provider_id: "acme", name: "Alpha", recommended: false }),
    um("acme/legacy.v2.5", { provider_id: "acme", name: "Legacy v2.5", recommended: false }),
    um("acme/zeta", { provider_id: "acme", name: "Zeta", recommended: true }),
    // beta: no recommendations → all models visible by fallback.
    um("beta/b1", { provider_id: "beta", name: "Beta One", recommended: false }),
    um("beta/b2", { provider_id: "beta", name: "Beta Two", recommended: false }),
    // fireworks: multi-segment id + full metadata.
    um("fireworks/accounts/fireworks/models/mimo-v2.5-pro", {
      provider_id: "fireworks",
      name: "MiMo v2.5 Pro",
      recommended: false,
      context_window: 1_048_576,
      output_limit: 16_384,
      pricing: {
        input_per_million: 3,
        output_per_million: 12,
        cache_read_per_million: 0.3,
        cache_write_per_million: 3.75,
      },
      reasoning: true,
      tool_call: true,
      attachment: true,
    }),
  ];
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

  it("default view hides non-recommended models when the provider has recommendations", () => {
    // Alpha (non-rec) vs Zeta (rec): Zeta name sorts AFTER Alpha.
    const models = [
      um("acme/alpha", { provider_id: "acme", name: "Alpha", recommended: false }),
      um("acme/zeta", { provider_id: "acme", name: "Zeta", recommended: true }),
    ];
    const defaults = providerDefaultModels(models);
    expect(defaults.map((m) => m.id)).toEqual(["acme/zeta"]);
  });

  it("default view shows ALL models for a provider with no recommendations", () => {
    const models = [
      um("beta/b1", { provider_id: "beta", name: "Beta One", recommended: false }),
      um("beta/b2", { provider_id: "beta", name: "Beta Two", recommended: false }),
    ];
    const defaults = providerDefaultModels(models);
    expect(defaults.map((m) => m.id).sort()).toEqual(["beta/b1", "beta/b2"]);
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

  it("puts recommended first even when its name sorts after a non-recommended one", () => {
    // Alpha (non-rec) vs Zeta (rec): pure alphabetical-only sort would put
    // Alpha first. Recommended-first must put Zeta before Alpha.
    const sorted = allModelsSorted([
      um("acme/alpha", { name: "Alpha", recommended: false }),
      um("acme/zeta", { name: "Zeta", recommended: true }),
    ]);
    expect(sorted.map((m) => m.id)).toEqual(["acme/zeta", "acme/alpha"]);
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
    const model = um("custom-model", {
      provider_id: undefined as unknown as UserModel["provider_id"],
    });
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

  it("orders recommended-first within a group even when its name sorts last", () => {
    // Alpha (non-rec) sorts before Zeta (rec) alphabetically. Recommended-first
    // must place Zeta first inside the acme group.
    const groups = groupModelsByProvider([
      um("acme/alpha", { name: "Alpha", recommended: false }),
      um("acme/zeta", { name: "Zeta", recommended: true }),
    ]);
    expect(groups[0]!.providerId).toBe("acme");
    expect(groups[0]!.models.map((m) => m.id)).toEqual(["acme/zeta", "acme/alpha"]);
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

/**
 * Component-level regression coverage for the browse-all / search model picker.
 * These tests drive the actual rendered UI through the AddModelButton picker:
 * open it, assert default rows, click Browse all, type a global search, select
 * a result, and inspect lane rendering / the save mutation payload.
 */
describe("ModelSection picker (browse/search)", () => {
  beforeEach(() => {
    vi.mocked(fetchUserConnectedModels).mockResolvedValue(pickerFixtures());
    vi.mocked(fetchUserModelSelection).mockResolvedValue({
      lanes: { plan: [], implement: [], review: [] },
      maxSessions: {},
      diverseReview: true,
      diverseRefinement: true,
    });
    // The save mutation's onSuccess reads saved.lanes/maxSessions — return a
    // valid echo so it does not crash.
    vi.mocked(saveUserModelSelection).mockImplementation(
      async (_targetId, lanes, maxSessions, options) => ({
        lanes,
        maxSessions,
        laneMaxSessions: options?.laneMaxSessions,
        diverseReview: options?.diverseReview ?? true,
        diverseRefinement: options?.diverseRefinement ?? true,
      }),
    );
  });

  it("default view shows recommended entries for curated providers and hides non-recommended ones", async () => {
    render(<ModelSection targetId="target-user" />);

    const addButtons = await screen.findAllByRole("button", { name: "Add model" });
    const user = userEvent.setup();
    await user.click(addButtons[0]!);

    // Wait for the dialog content to render.
    await screen.findByText("Add a model");

    // Recommended "Zeta" (acme) is visible by default; non-recommended "Alpha" is NOT.
    expect(screen.getByText("Zeta")).toBeInTheDocument();
    expect(screen.queryByText("Alpha")).not.toBeInTheDocument();
  });

  it("default view shows ALL models for a provider with no recommendations (fallback)", async () => {
    render(<ModelSection targetId="target-user" />);

    const addButtons = await screen.findAllByRole("button", { name: "Add model" });
    const user = userEvent.setup();
    await user.click(addButtons[0]!);

    await screen.findByText("Add a model");

    // beta has no recommendations → both models visible by fallback.
    expect(screen.getByText("Beta One")).toBeInTheDocument();
    expect(screen.getByText("Beta Two")).toBeInTheDocument();
  });

  it("Browse all exposes the hidden non-recommended curated-provider model", async () => {
    render(<ModelSection targetId="target-user" />);

    const addButtons = await screen.findAllByRole("button", { name: "Add model" });
    const user = userEvent.setup();
    await user.click(addButtons[0]!);

    await screen.findByText("Add a model");

    // Alpha is hidden by default; the "Browse all Acme models" affordance reveals it.
    expect(screen.queryByText("Alpha")).not.toBeInTheDocument();
    const browseAll = await screen.findByRole("button", { name: /browse all acme models/i });
    await user.click(browseAll);

    expect(await screen.findByText("Alpha")).toBeInTheDocument();
  });

  it("global search exposes the hidden non-recommended curated-provider model", async () => {
    render(<ModelSection targetId="target-user" />);

    const addButtons = await screen.findAllByRole("button", { name: "Add model" });
    const user = userEvent.setup();
    await user.click(addButtons[0]!);

    const search = await screen.findByPlaceholderText("Search models…");
    await user.type(search, "alpha");

    // Searching surfaces the hidden non-recommended model.
    expect(await screen.findByText("Alpha")).toBeInTheDocument();
  });

  it("browse/search results stay grouped by provider", async () => {
    render(<ModelSection targetId="target-user" />);

    const addButtons = await screen.findAllByRole("button", { name: "Add model" });
    const user = userEvent.setup();
    await user.click(addButtons[0]!);

    const search = await screen.findByPlaceholderText("Search models…");
    // A broad query that matches across multiple providers.
    await user.type(search, "a");

    // Provider group headings are present for matched providers.
    expect(screen.getByText("Acme")).toBeInTheDocument();
    expect(screen.getByText("Beta")).toBeInTheDocument();
  });

  it("browse/search ordering is recommended-first, not alphabetical-only (Alpha vs Zeta)", async () => {
    render(<ModelSection targetId="target-user" />);

    const addButtons = await screen.findAllByRole("button", { name: "Add model" });
    const user = userEvent.setup();
    await user.click(addButtons[0]!);

    const search = await screen.findByPlaceholderText("Search models…");
    // Query "a" matches both "Alpha" and "Zeta" (via the acme provider id).
    await user.type(search, "acme");

    // Wait for the acme group to render with both items.
    const acmeGroup = await screen.findByText("Acme");
    const groupEl = acmeGroup.closest("[data-slot='command-group']");
    expect(groupEl).not.toBeNull();
    const items = within(groupEl as HTMLElement).getAllByRole("button");
    // First item must be the recommended "Zeta" even though "Alpha" sorts first
    // alphabetically. (Filter out the "Browse all" button by checking names.)
    const names = items.map((el) => el.textContent ?? "");
    // The recommended Zeta appears before Alpha in the listed model rows.
    const zetaIdx = names.findIndex((n) => n.includes("Zeta"));
    const alphaIdx = names.findIndex((n) => n.includes("Alpha"));
    expect(zetaIdx).toBeGreaterThanOrEqual(0);
    expect(alphaIdx).toBeGreaterThanOrEqual(0);
    expect(zetaIdx).toBeLessThan(alphaIdx);
  });

  it("renders metadata chips for browse/search rows (context, pricing, capabilities)", async () => {
    render(<ModelSection targetId="target-user" />);

    const addButtons = await screen.findAllByRole("button", { name: "Add model" });
    const user = userEvent.setup();
    await user.click(addButtons[0]!);

    const search = await screen.findByPlaceholderText("Search models…");
    await user.type(search, "mimo");

    // The multi-segment fireworks model with full metadata is found.
    const mimoRow = await screen.findByText("MiMo v2.5 Pro");
    expect(mimoRow).toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "Add a model" })).toHaveClass(
      "sm:max-w-xl",
    );

    // Keep the model identity on the primary line and the dense technical
    // metadata on its own wrapping line so it cannot collapse the name.
    const item = mimoRow.closest("[data-slot='command-item']");
    expect(item).not.toBeNull();
    expect(item).toHaveClass("items-start", "hover:bg-primary/10");
    expect(mimoRow).toHaveAttribute("data-slot", "model-picker-name");
    expect(mimoRow).toHaveClass("whitespace-normal", "font-medium");
    const metadata = (item as HTMLElement).querySelector(
      "[data-slot='model-picker-metadata']",
    );
    expect(metadata).not.toBeNull();
    expect(metadata).toHaveClass("block", "whitespace-normal", "text-xs");
    const itemText = (item as HTMLElement).textContent ?? "";
    expect(itemText).toContain("1M ctx");
    expect(itemText).toContain("$3.00/$12.00 per M tok");
    expect(itemText).toContain("reasoning");
    expect(itemText).toContain("tools");
    expect(itemText).toContain("vision");
    expect(metadata).toHaveAttribute(
      "title",
      "1M ctx · $3.00/$12.00 per M tok · reasoning tools vision",
    );
  });

  it("renders the Recommended badge for a recommended model in browse/search", async () => {
    render(<ModelSection targetId="target-user" />);

    const addButtons = await screen.findAllByRole("button", { name: "Add model" });
    const user = userEvent.setup();
    await user.click(addButtons[0]!);

    const search = await screen.findByPlaceholderText("Search models…");
    await user.type(search, "zeta");

    const zetaRow = await screen.findByText("Zeta");
    const item = zetaRow.closest("[data-slot='command-item']");
    expect(item).not.toBeNull();
    expect(within(item as HTMLElement).getByText("Recommended")).toBeInTheDocument();
  });

  it("a refreshed connected-model response while mounted makes a new non-recommended multi-segment model addable", async () => {
    const refreshedId = "fireworks/accounts/fireworks/models/new-refreshed-v1";
    const refreshedName = "New Refreshed v1";

    const initialModels = [
      um("acme/alpha", { provider_id: "acme", name: "Alpha", recommended: false }),
      um("acme/zeta", { provider_id: "acme", name: "Zeta", recommended: true }),
    ];

    const refreshedModels = [
      ...initialModels,
      um(refreshedId, {
        provider_id: "fireworks",
        name: refreshedName,
        recommended: false,
      }),
    ];

    let callCount = 0;
    vi.mocked(fetchUserConnectedModels).mockImplementation(async () => {
      callCount += 1;
      return callCount === 1 ? initialModels : refreshedModels;
    });
    vi.mocked(saveUserModelSelection).mockClear();

    render(<ModelSection targetId="target-user" />);

    const user = userEvent.setup();
    const refreshButton = await screen.findByRole("button", { name: "Refresh models" });

    // Before refresh the new model is not reachable via search.
    const addButtonsBefore = await screen.findAllByRole("button", { name: "Add model" });
    await user.click(addButtonsBefore[0]!);
    await screen.findByText("Add a model");
    const searchBefore = await screen.findByPlaceholderText("Search models…");
    await user.type(searchBefore, "new-refreshed");
    expect(screen.queryByText(refreshedName)).not.toBeInTheDocument();

    // Close the picker and trigger the settings-surface refetch path.
    await user.click(await screen.findByRole("button", { name: "Close" }));
    await waitFor(() => {
      expect(screen.queryByText("Add a model")).not.toBeInTheDocument();
    });
    await user.click(refreshButton);

    // Wait for the refetch to complete by observing the local call count.
    await waitFor(() => {
      expect(callCount).toBe(2);
    });

    // After refresh the same mounted component can browse/search the new model.
    const addButtonsAfter = await screen.findAllByRole("button", { name: "Add model" });
    await user.click(addButtonsAfter[0]!);
    await screen.findByText("Add a model");
    const searchAfter = await screen.findByPlaceholderText("Search models…");
    await user.type(searchAfter, "new-refreshed");
    const refreshedRow = await screen.findByText(refreshedName);
    const item = refreshedRow.closest("[data-slot='command-item']")!;
    await user.click(item);

    await waitFor(() => {
      expect(screen.getByText(refreshedName)).toBeInTheDocument();
    });

    // Save and assert the exact full multi-segment id reaches the mutation.
    const saveButton = screen.getByRole("button", { name: "Save" });
    await user.click(saveButton);

    await waitFor(() => {
      expect(saveUserModelSelection).toHaveBeenCalledTimes(1);
    });
    const call = vi.mocked(saveUserModelSelection).mock.calls[0]!;
    const lanes = call[1];
    const allIds = [...lanes.plan, ...lanes.implement, ...lanes.review];
    expect(allIds).toContain(refreshedId);
    expect(call[2]).toHaveProperty(refreshedId);
  });

  it("selecting a multi-segment model id adds the exact full id without truncation", async () => {
    vi.mocked(saveUserModelSelection).mockClear();
    render(<ModelSection targetId="target-user" />);

    const addButtons = await screen.findAllByRole("button", { name: "Add model" });
    const user = userEvent.setup();
    await user.click(addButtons[0]!);

    const search = await screen.findByPlaceholderText("Search models…");
    await user.type(search, "mimo");

    const mimoRow = await screen.findByText("MiMo v2.5 Pro");
    // Click the command item to select it.
    const item = mimoRow.closest("[data-slot='command-item']")!;
    await user.click(item);

    // The full multi-segment id now appears in the lane as a selected model.
    const fullId = "fireworks/accounts/fireworks/models/mimo-v2.5-pro";
    await waitFor(() => {
      expect(screen.getByText("MiMo v2.5 Pro")).toBeInTheDocument();
    });
    // The provider display name for fireworks renders next to it.
    expect(screen.getByText("Fireworks")).toBeInTheDocument();

    // Save and assert the mutation payload carries the exact full id.
    const saveButton = screen.getByRole("button", { name: "Save" });
    await user.click(saveButton);

    await waitFor(() => {
      expect(saveUserModelSelection).toHaveBeenCalledTimes(1);
    });
    const call = vi.mocked(saveUserModelSelection).mock.calls[0]!;
    // args: (targetUserId, lanes, maxSessions, diverseReview, diverseRefinement)
    const lanes = call[1];
    const allIds = [...lanes.plan, ...lanes.implement, ...lanes.review];
    expect(allIds).toContain(fullId);
    // maxSessions keys by the full id.
    expect(call[2]).toHaveProperty(fullId);
  });

  it("selecting a dotted non-recommended id adds the exact full id without splitting", async () => {
    vi.mocked(saveUserModelSelection).mockClear();
    render(<ModelSection targetId="target-user" />);

    const addButtons = await screen.findAllByRole("button", { name: "Add model" });
    const user = userEvent.setup();
    // Use the Implement lane so it does not share the existing plan lane shape.
    await user.click(addButtons[1]!);

    const search = await screen.findByPlaceholderText("Search models…");
    await user.type(search, "legacy.v2.5");

    const legacyRow = await screen.findByText("Legacy v2.5");
    const item = legacyRow.closest("[data-slot='command-item']")!;
    await user.click(item);

    const dottedId = "acme/legacy.v2.5";
    await waitFor(() => {
      expect(screen.getByText("Legacy v2.5")).toBeInTheDocument();
    });

    const saveButton = screen.getByRole("button", { name: "Save" });
    await user.click(saveButton);

    await waitFor(() => {
      expect(saveUserModelSelection).toHaveBeenCalledTimes(1);
    });
    const call = vi.mocked(saveUserModelSelection).mock.calls[0]!;
    const lanes = call[1];
    const allIds = [...lanes.plan, ...lanes.implement, ...lanes.review];
    expect(allIds).toContain(dottedId);
    expect(call[2]).toHaveProperty(dottedId);
  });

  it("a demoted connected non-recommended model is still reachable through browse and search", async () => {
    render(<ModelSection targetId="target-user" />);

    const addButtons = await screen.findAllByRole("button", { name: "Add model" });
    const user = userEvent.setup();
    await user.click(addButtons[0]!);

    await screen.findByText("Add a model");

    // Default view hides non-recommended Legacy v2.5 (acme has a recommended Zeta).
    expect(screen.queryByText("Legacy v2.5")).not.toBeInTheDocument();

    // Browse all reveals it.
    const browseAll = await screen.findByRole("button", { name: /browse all acme models/i });
    await user.click(browseAll);
    expect(await screen.findByText("Legacy v2.5")).toBeInTheDocument();

    // Close the picker, reopen it, then search by the dotted suffix to prove it
    // remains findable.
    await user.click(await screen.findByRole("button", { name: "Close" }));
    await waitFor(() => {
      expect(screen.queryByText("Add a model")).not.toBeInTheDocument();
    });

    await user.click(addButtons[0]!);
    await screen.findByText("Add a model");
    const search = await screen.findByPlaceholderText("Search models…");
    await user.type(search, "legacy.v2.5");
    expect(await screen.findByText("Legacy v2.5")).toBeInTheDocument();
  });
});

describe("ModelSection", () => {
  beforeEach(() => {
    // Reset to the simple default fixtures for the smoke tests.
    vi.mocked(fetchUserConnectedModels).mockResolvedValue([
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
    ]);
    vi.mocked(fetchUserModelSelection).mockResolvedValue({
      lanes: { plan: ["openai/gpt-5"], implement: [], review: [] },
      maxSessions: { "openai/gpt-5": 3 },
      diverseReview: true,
      diverseRefinement: true,
    });
    vi.mocked(saveUserModelSelection).mockClear();
    vi.mocked(saveUserModelSelection).mockImplementation(
      async (_targetId, lanes, maxSessions, options) => ({
        lanes,
        maxSessions,
        laneMaxSessions: options?.laneMaxSessions,
        diverseReview: options?.diverseReview ?? true,
        diverseRefinement: options?.diverseRefinement ?? true,
      }),
    );
  });

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
    expect(
      screen.getByRole("combobox", { name: "Plan parallel agents" }),
    ).toHaveTextContent("1");
    expect(
      screen.getByText("Autonomous planning, Architect, Lead, Refinement"),
    ).toBeInTheDocument();
    expect(screen.queryByText("Planner, Architect, Chat")).not.toBeInTheDocument();
    expect(screen.getByText(/automatically raises a too-low/i)).toBeInTheDocument();
    // Each lane exposes its own Add model trigger.
    expect(
      screen.getAllByRole("button", { name: "Add model" }).length,
    ).toBeGreaterThanOrEqual(3);
  });

  it("keeps legacy unset lane limits omitted on an unrelated Settings save", async () => {
    const user = userEvent.setup();
    render(<ModelSection targetId="target-user" />);

    const modelCap = await screen.findByDisplayValue("3");
    await user.clear(modelCap);
    await user.type(modelCap, "4");
    await user.tab();
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(saveUserModelSelection).toHaveBeenCalledOnce());
    expect(saveUserModelSelection).toHaveBeenCalledWith(
      "target-user",
      { plan: ["openai/gpt-5"], implement: [], review: [] },
      { "openai/gpt-5": 4 },
      {
        diverseReview: true,
        diverseRefinement: true,
      },
    );
    expect(
      vi.mocked(saveUserModelSelection).mock.calls[0]?.[3],
    ).not.toHaveProperty("laneMaxSessions");
  });

  it("keeps persisted lane limits effective when saving another setting", async () => {
    vi.mocked(fetchUserModelSelection).mockResolvedValue({
      lanes: {
        plan: ["openai/gpt-5"],
        implement: ["openai/gpt-5"],
        review: ["openai/gpt-5"],
      },
      maxSessions: { "openai/gpt-5": 1 },
      laneMaxSessions: { plan: 2, implement: 3, review: 1 },
      diverseReview: true,
      diverseRefinement: true,
    });
    const user = userEvent.setup();
    render(<ModelSection targetId="target-user" />);

    const modelCap = (await screen.findAllByDisplayValue("1")).find((element) =>
      element.matches("input[data-slot='input']"),
    );
    expect(modelCap).toBeDefined();
    if (!modelCap) throw new Error("missing model Sessions input");
    await user.clear(modelCap);
    await user.type(modelCap, "2");
    await user.tab();
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(saveUserModelSelection).toHaveBeenCalledOnce());
    expect(vi.mocked(saveUserModelSelection).mock.calls[0]?.[2]).toEqual({
      "openai/gpt-5": 6,
    });
    expect(
      vi.mocked(saveUserModelSelection).mock.calls[0]?.[3],
    ).not.toHaveProperty("laneMaxSessions");
  });

  it("saves 1–10 lane limits and raises one shared model cap to their sum", async () => {
    vi.mocked(fetchUserModelSelection).mockResolvedValue({
      lanes: {
        plan: ["openai/gpt-5"],
        implement: ["openai/gpt-5"],
        review: ["openai/gpt-5"],
      },
      maxSessions: { "openai/gpt-5": 1 },
      laneMaxSessions: { plan: 1, implement: 1, review: 1 },
      diverseReview: true,
      diverseRefinement: true,
    });
    const user = userEvent.setup();

    render(<ModelSection targetId="target-user" />);
    await screen.findAllByText("GPT-5");
    await chooseLaneLimit(user, "Plan", 2);
    await chooseLaneLimit(user, "Implement", 4);
    await chooseLaneLimit(user, "Review", 3);
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(saveUserModelSelection).toHaveBeenCalledOnce());
    expect(saveUserModelSelection).toHaveBeenCalledWith(
      "target-user",
      {
        plan: ["openai/gpt-5"],
        implement: ["openai/gpt-5"],
        review: ["openai/gpt-5"],
      },
      { "openai/gpt-5": 9 },
      {
        diverseReview: true,
        diverseRefinement: true,
        laneMaxSessions: { plan: 2, implement: 4, review: 3 },
      },
    );
  });

  it("floors distinct model caps only by the lanes each model serves", async () => {
    vi.mocked(fetchUserModelSelection).mockResolvedValue({
      lanes: {
        plan: ["openai/gpt-5"],
        implement: ["anthropic/claude-sonnet-4"],
        review: ["anthropic/claude-sonnet-4"],
      },
      maxSessions: {
        "openai/gpt-5": 1,
        "anthropic/claude-sonnet-4": 1,
      },
      laneMaxSessions: { plan: 1, implement: 1, review: 1 },
      diverseReview: true,
      diverseRefinement: true,
    });
    const user = userEvent.setup();

    render(<ModelSection targetId="target-user" />);
    await screen.findAllByText("GPT-5");
    await chooseLaneLimit(user, "Plan", 2);
    await chooseLaneLimit(user, "Implement", 3);
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(saveUserModelSelection).toHaveBeenCalledOnce());
    expect(vi.mocked(saveUserModelSelection).mock.calls[0]?.[2]).toEqual({
      "openai/gpt-5": 2,
      "anthropic/claude-sonnet-4": 4,
    });
    expect(vi.mocked(saveUserModelSelection).mock.calls[0]?.[3]).toMatchObject({
      laneMaxSessions: { plan: 2, implement: 3, review: 1 },
    });
  });

  it("hides advanced diversity toggles in onboarding mode", async () => {
    render(<ModelSection targetId="target-user" onboarding />);

    expect(await screen.findByText("GPT-5")).toBeInTheDocument();
    expect(
      screen.queryByRole("switch", { name: "Thorough review" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("switch", { name: "Diverse refinement" }),
    ).not.toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Plan" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Implement" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Review" })).toBeInTheDocument();
  });
});

async function chooseLaneLimit(
  user: ReturnType<typeof userEvent.setup>,
  lane: string,
  value: number,
) {
  await user.click(
    screen.getByRole("combobox", { name: `${lane} parallel agents` }),
  );
  await user.click(await screen.findByRole("option", { name: String(value) }));
}
