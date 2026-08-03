import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  hasAnyProposal: vi.fn(),
}));

vi.mock("@/api/proposals", () => ({
  hasAnyProposal: mocks.hasAnyProposal,
}));

import { useProposalGateStore } from "./proposalGateStore";

describe("proposalGateStore", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useProposalGateStore.setState({ hasProposal: null, error: null });
  });

  it("keeps onboarding open until at least one proposal exists", async () => {
    mocks.hasAnyProposal.mockResolvedValue(false);

    await useProposalGateStore.getState().refresh();

    expect(useProposalGateStore.getState()).toMatchObject({
      hasProposal: false,
      error: null,
    });
  });

  it("marks the gate complete after the starter proposal is created", () => {
    useProposalGateStore.getState().markComplete();

    expect(useProposalGateStore.getState()).toMatchObject({
      hasProposal: true,
      error: null,
    });
  });

  it("fails closed when proposal readiness cannot be loaded", async () => {
    mocks.hasAnyProposal.mockRejectedValue(new Error("offline"));

    await useProposalGateStore.getState().refresh();

    expect(useProposalGateStore.getState()).toMatchObject({
      hasProposal: null,
      error: "offline",
    });
  });
});
