import { describe, expect, it, vi } from "vitest";

import type { UserModel } from "@/api/userConfig";
import { render, screen } from "@/test/test-utils";

import { ModelSection, stripProviderPrefix } from "./ModelSection";

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
