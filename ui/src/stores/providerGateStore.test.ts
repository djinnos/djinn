import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  fetchCredentialList: vi.fn(),
}));

vi.mock("@/api/server", () => ({
  fetchCredentialList: mocks.fetchCredentialList,
}));

import { useProviderGateStore } from "./providerGateStore";

describe("providerGateStore", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useProviderGateStore.setState({ hasProvider: null });
  });

  it("requires at least one valid provider credential", async () => {
    mocks.fetchCredentialList.mockResolvedValue([
      { provider_id: "openai", configured: true, valid: false },
      { provider_id: "anthropic", configured: true, valid: true },
    ]);

    await useProviderGateStore.getState().refresh();

    expect(useProviderGateStore.getState().hasProvider).toBe(true);
  });

  it("fails closed when provider readiness cannot be loaded", async () => {
    mocks.fetchCredentialList.mockRejectedValue(new Error("offline"));

    await useProviderGateStore.getState().refresh();

    expect(useProviderGateStore.getState().hasProvider).toBe(false);
  });
});
