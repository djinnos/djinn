import { beforeEach, describe, expect, it, vi } from "vitest";

import { callMcpTool } from "@/api/mcpClient";

import {
  fetchUserSettings,
  parseLaneMaxSessions,
  patchUserSettings,
} from "./userSettings";

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: vi.fn(),
}));

describe("lane max sessions API mapping", () => {
  beforeEach(() => {
    vi.mocked(callMcpTool).mockReset();
  });

  it("preserves the distinction between unset and an explicit full limit", async () => {
    vi.mocked(callMcpTool)
      .mockResolvedValueOnce({
        ok: true,
        lanes: {},
        max_sessions: {},
        lane_max_sessions: null,
      } as never)
      .mockResolvedValueOnce({
        ok: true,
        lanes: {},
        max_sessions: {},
        lane_max_sessions: { plan: 2, implement: 3, review: 1 },
      } as never);

    await expect(fetchUserSettings()).resolves.toMatchObject({
      laneMaxSessions: undefined,
    });
    await expect(fetchUserSettings()).resolves.toMatchObject({
      laneMaxSessions: { plan: 2, implement: 3, review: 1 },
    });
  });

  it("rejects partial, non-integer, and out-of-range wire values", () => {
    expect(parseLaneMaxSessions({ plan: 1, implement: 1 })).toBeUndefined();
    expect(
      parseLaneMaxSessions({ plan: 1, implement: 1.5, review: 1 }),
    ).toBeUndefined();
    expect(
      parseLaneMaxSessions({ plan: 1, implement: 11, review: 1 }),
    ).toBeUndefined();
  });

  it("passes an explicit lane limit through user_settings_set", async () => {
    vi.mocked(callMcpTool).mockResolvedValue({
      ok: true,
      lanes: {},
      max_sessions: {},
      lane_max_sessions: { plan: 2, implement: 3, review: 1 },
    } as never);

    await patchUserSettings({
      laneMaxSessions: { plan: 2, implement: 3, review: 1 },
    });

    expect(callMcpTool).toHaveBeenCalledWith("user_settings_set", {
      lane_max_sessions: { plan: 2, implement: 3, review: 1 },
    });
  });
});
