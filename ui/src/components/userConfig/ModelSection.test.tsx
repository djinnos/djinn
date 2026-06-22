import { describe, expect, it, vi } from "vitest";

import type { UserModel } from "@/api/userConfig";
import { render, screen } from "@/test/test-utils";

import { ModelSection } from "./ModelSection";
import { pickableModels, stripProviderPrefix } from "./modelPicker";

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
