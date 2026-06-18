import { describe, expect, it, vi } from "vitest";

import type { AutomationModel } from "@/api/automationConfig";
import { render, screen } from "@/test/test-utils";

import { ModelSection, stripProviderPrefix } from "./ModelSection";

vi.mock("@/lib/toast", () => ({
  showToast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

vi.mock("@/api/automationConfig", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/automationConfig")>();
  return {
    ...actual,
    fetchAutomationConnectedModels: vi.fn(async () => [
      {
        id: "openai/gpt-5",
        name: "GPT-5",
        provider_id: "openai",
        tool_call: true,
      } as AutomationModel,
      {
        id: "anthropic/claude-sonnet-4",
        name: "Claude Sonnet 4",
        provider_id: "anthropic",
        tool_call: true,
      } as AutomationModel,
    ]),
    fetchAutomationModelSelection: vi.fn(async () => ({
      models: ["openai/gpt-5"],
      maxSessions: { "openai/gpt-5": 3 },
    })),
    saveAutomationModelSelection: vi.fn(),
  };
});

describe("ModelSection", () => {
  it("strips provider prefixes for fallback model display", () => {
    expect(stripProviderPrefix("openai/gpt-5")).toBe("gpt-5");
    expect(stripProviderPrefix("custom-model")).toBe("custom-model");
  });

  it("smoke-renders selected automation model settings", async () => {
    render(<ModelSection targetId="automation-user" />);

    expect(screen.getByRole("heading", { name: "Models" })).toBeInTheDocument();
    expect(await screen.findByText("GPT-5")).toBeInTheDocument();
    expect(screen.getByText("OpenAI")).toBeInTheDocument();
    expect(screen.getByDisplayValue("3")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add model" })).toBeInTheDocument();
  });
});
