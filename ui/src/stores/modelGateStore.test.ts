import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  fetchUserModelSelection: vi.fn(),
}));

vi.mock("@/api/userConfig", () => ({
  SELF_TARGET: "__self__",
  fetchUserModelSelection: mocks.fetchUserModelSelection,
}));

import { useModelGateStore } from "./modelGateStore";

function selection(
  lanes: {
    plan: string[];
    implement: string[];
    review: string[];
  },
  laneLocked = false,
) {
  return {
    lanes,
    maxSessions: {},
    diverseReview: true,
    diverseRefinement: true,
    laneLocked,
  };
}

describe("modelGateStore", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useModelGateStore.setState({ hasModels: null });
  });

  it("keeps onboarding open when only some model roles are configured", async () => {
    mocks.fetchUserModelSelection.mockResolvedValue(
      selection({
        plan: ["openai/gpt-5.5"],
        implement: ["openai/gpt-5.3-codex"],
        review: [],
      }),
    );

    await useModelGateStore.getState().refresh();

    expect(mocks.fetchUserModelSelection).toHaveBeenCalledWith("__self__");
    expect(useModelGateStore.getState().hasModels).toBe(false);
  });

  it("closes onboarding only after every model role is configured", async () => {
    mocks.fetchUserModelSelection.mockResolvedValue(
      selection({
        plan: ["openai/gpt-5.5"],
        implement: ["openai/gpt-5.3-codex"],
        review: ["openai/gpt-5.5"],
      }),
    );

    await useModelGateStore.getState().refresh();

    expect(useModelGateStore.getState().hasModels).toBe(true);
  });

  it("keeps locked-but-empty org lanes blocked for an admin to fix", async () => {
    mocks.fetchUserModelSelection.mockResolvedValue(
      selection({ plan: [], implement: [], review: [] }, true),
    );

    await useModelGateStore.getState().refresh();

    expect(useModelGateStore.getState().hasModels).toBe(false);
  });

  it("fails closed when model settings cannot be loaded", async () => {
    mocks.fetchUserModelSelection.mockRejectedValue(new Error("offline"));

    await useModelGateStore.getState().refresh();

    expect(useModelGateStore.getState().hasModels).toBe(false);
  });
});
