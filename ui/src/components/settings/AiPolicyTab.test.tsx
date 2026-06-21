import { describe, expect, it, vi } from "vitest";

import type { OrgPolicy } from "@/api/orgPolicy";
import { render, screen, userEvent, waitFor } from "@/test/test-utils";

import { AiPolicyTab } from "./AiPolicyTab";

vi.mock("@/lib/toast", () => ({
  showToast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

const samplePolicy: OrgPolicy = {
  subscriptions: [
    { id: "chatgpt_codex", name: "ChatGPT Codex", jurisdiction: "us", blocked: false },
    { id: "minimax-coding-plan", name: "MiniMax Coding Plan", jurisdiction: "cn", blocked: false },
    { id: "kimi-for-coding", name: "Kimi for Coding", jurisdiction: "cn", blocked: true },
  ],
  blockedSubscriptions: ["kimi-for-coding"],
  defaultLanes: { plan: [], implement: [], review: [] },
  lockLevel: "flexible",
};

const fetchOrgPolicy = vi.fn(async () => samplePolicy);
const saveOrgPolicy = vi.fn(async () => samplePolicy);

vi.mock("@/api/orgPolicy", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/orgPolicy")>();
  return {
    ...actual,
    fetchOrgPolicy: (...args: unknown[]) => fetchOrgPolicy(...(args as [])),
    saveOrgPolicy: (...args: unknown[]) => saveOrgPolicy(...(args as [])),
  };
});

describe("AiPolicyTab", () => {
  it("renders subscriptions grouped by jurisdiction with block toggles", async () => {
    render(<AiPolicyTab />);

    expect(await screen.findByText("ChatGPT Codex")).toBeInTheDocument();
    expect(screen.getByText("MiniMax Coding Plan")).toBeInTheDocument();
    // Jurisdiction group labels are present.
    expect(screen.getByText("United States")).toBeInTheDocument();
    expect(screen.getByText("China")).toBeInTheDocument();
    // The already-blocked sub shows as Blocked.
    expect(screen.getAllByText("Blocked").length).toBeGreaterThan(0);
  });

  it("one-click blocks all China-hosted subscriptions", async () => {
    render(<AiPolicyTab />);
    const button = await screen.findByTestId("block-all-china");
    await userEvent.click(button);

    await waitFor(() => expect(saveOrgPolicy).toHaveBeenCalled());
    const patch = saveOrgPolicy.mock.calls.at(-1)?.[0] as
      | { blockedSubscriptions?: string[] }
      | undefined;
    expect(patch?.blockedSubscriptions).toEqual(
      expect.arrayContaining(["minimax-coding-plan", "kimi-for-coding"]),
    );
  });

  it("toggles the lane lock level", async () => {
    render(<AiPolicyTab />);
    const lockSwitch = await screen.findByLabelText("Lock member lane assignment");
    await userEvent.click(lockSwitch);
    await waitFor(() => expect(saveOrgPolicy).toHaveBeenCalled());
    const patch = saveOrgPolicy.mock.calls.at(-1)?.[0] as { lockLevel?: string } | undefined;
    expect(patch?.lockLevel).toBe("locked");
  });
});
