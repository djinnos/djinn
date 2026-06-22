import { describe, expect, it, vi } from "vitest";

import type { UserModel } from "@/api/userConfig";
import type { OrgPolicy } from "@/api/orgPolicy";
import { render, screen, userEvent, waitFor } from "@/test/test-utils";

import { OrgDefaultLanesSection } from "./OrgDefaultLanesSection";

vi.mock("@/lib/toast", () => ({
  showToast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

vi.mock("@/api/userConfig", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/userConfig")>();
  return {
    ...actual,
    fetchUserConnectedModels: vi.fn(
      async () =>
        [
          { id: "openai/gpt-5", name: "GPT-5", provider_id: "openai", tool_call: true },
          {
            id: "anthropic/claude-sonnet-4",
            name: "Claude Sonnet 4",
            provider_id: "anthropic",
            tool_call: true,
          },
        ] as UserModel[],
    ),
  };
});

const policy: OrgPolicy = {
  subscriptions: [],
  blockedSubscriptions: [],
  defaultLanes: { plan: ["openai/gpt-5"], implement: [], review: [] },
  lockLevel: "flexible",
};

describe("OrgDefaultLanesSection", () => {
  it("renders the org-default lanes and seeded model", async () => {
    render(<OrgDefaultLanesSection policy={policy} saving={false} onSave={vi.fn()} />);

    expect(await screen.findByText("Org-default model roles")).toBeInTheDocument();
    // Lane headings appear after the admin's connected models load.
    expect(await screen.findByText("Plan")).toBeInTheDocument();
    expect(screen.getByText("Implement")).toBeInTheDocument();
    expect(screen.getByText("Review")).toBeInTheDocument();
    // The seeded plan-lane model renders.
    expect(await screen.findByText("GPT-5")).toBeInTheDocument();
  });

  it("removing a model dirties the editor and saves defaultLanes", async () => {
    const onSave = vi.fn();
    render(<OrgDefaultLanesSection policy={policy} saving={false} onSave={onSave} />);

    const removeBtn = await screen.findByRole("button", { name: /remove gpt-5/i });
    await userEvent.click(removeBtn);

    const saveBtn = await screen.findByRole("button", { name: /save default/i });
    await userEvent.click(saveBtn);

    await waitFor(() => expect(onSave).toHaveBeenCalled());
    const patch = onSave.mock.calls.at(-1)?.[0] as { defaultLanes?: { plan: string[] } };
    expect(patch.defaultLanes?.plan).toEqual([]);
  });
});
