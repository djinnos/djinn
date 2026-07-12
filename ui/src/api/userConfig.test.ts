import { beforeEach, describe, expect, it, vi } from "vitest";

import { callMcpTool } from "@/api/mcpClient";
import {
  SELF_TARGET,
  setUserCredential,
  startUserOAuth,
} from "@/api/userConfig";

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: vi.fn(),
}));

const callMcpToolMock = vi.mocked(callMcpTool);

describe("setUserCredential", () => {
  beforeEach(() => {
    callMcpToolMock.mockReset();
  });

  it("omits the admin-only target_user_id for the signed-in user", async () => {
    callMcpToolMock.mockResolvedValueOnce({ ok: true, success: true } as never);

    await setUserCredential({
      targetUserId: SELF_TARGET,
      providerId: "anthropic",
      keyName: "ANTHROPIC_API_KEY",
      apiKey: "sk-ant-test",
    });

    expect(callMcpToolMock).toHaveBeenCalledWith("credential_set", {
      provider_id: "anthropic",
      key_name: "ANTHROPIC_API_KEY",
      api_key: "sk-ant-test",
    });
  });

  it("includes target_user_id when an admin configures another user", async () => {
    callMcpToolMock.mockResolvedValueOnce({ ok: true, success: true } as never);

    await setUserCredential({
      targetUserId: "target-user-1",
      providerId: "anthropic",
      keyName: "ANTHROPIC_API_KEY",
      apiKey: "sk-ant-test",
    });

    expect(callMcpToolMock).toHaveBeenCalledWith("credential_set", {
      target_user_id: "target-user-1",
      provider_id: "anthropic",
      key_name: "ANTHROPIC_API_KEY",
      api_key: "sk-ant-test",
    });
  });
});

describe("startUserOAuth", () => {
  beforeEach(() => {
    callMcpToolMock.mockReset();
  });

  it("omits the admin-only target_user_id for the signed-in user", async () => {
    callMcpToolMock.mockResolvedValueOnce({
      ok: false,
      error: "Could not persist Codex credentials",
    } as never);

    await expect(startUserOAuth(SELF_TARGET, "openai")).resolves.toEqual({
      kind: "error",
      message: "Could not persist Codex credentials",
    });
    expect(callMcpToolMock).toHaveBeenCalledWith("provider_oauth_start", {
      provider_id: "openai",
    });
  });

  it("includes target_user_id when an admin configures another user", async () => {
    callMcpToolMock.mockResolvedValueOnce({ ok: true, pending: false } as never);

    await expect(startUserOAuth("target-user-1", "openai")).resolves.toEqual({
      kind: "connected",
    });
    expect(callMcpToolMock).toHaveBeenCalledWith("provider_oauth_start", {
      target_user_id: "target-user-1",
      provider_id: "openai",
    });
  });
});
