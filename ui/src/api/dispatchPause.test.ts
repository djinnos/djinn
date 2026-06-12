import { beforeEach, describe, expect, it, vi } from "vitest";

import { callMcpTool } from "@/api/mcpClient";
import {
  fetchDispatchPauseStatus,
  fetchGlobalDispatchPauseStatus,
  fetchProjectDispatchPauseStatus,
  fetchUserDispatchPauseStatus,
} from "@/api/dispatchPause";

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: vi.fn(),
}));

beforeEach(() => {
  vi.mocked(callMcpTool).mockReset();
  vi.mocked(callMcpTool).mockResolvedValue({
    ok: true,
    state: { projects: {}, users: {} },
  });
});

describe("dispatch pause status API", () => {
  it("fetches all dispatch pause status with exact empty arguments", async () => {
    await fetchDispatchPauseStatus();

    expect(vi.mocked(callMcpTool)).toHaveBeenCalledTimes(1);
    expect(vi.mocked(callMcpTool)).toHaveBeenCalledWith("dispatch_pause_status", {});
  });

  it("does not forward unknown mutation-like fields to status reads", async () => {
    await fetchDispatchPauseStatus({
      paused: true,
      reason: "maintenance",
      resume: true,
    } as never);

    expect(vi.mocked(callMcpTool)).toHaveBeenCalledTimes(1);
    expect(vi.mocked(callMcpTool)).toHaveBeenCalledWith("dispatch_pause_status", {});
    expect(vi.mocked(callMcpTool)).not.toHaveBeenCalledWith(
      expect.stringMatching(/^dispatch_(pause|resume)$/),
      expect.anything(),
    );
  });

  it("strips blank targets while preserving only exact status scope arguments", async () => {
    await fetchDispatchPauseStatus({
      scope: "project",
      target_id: "   ",
      stray: "ignored",
    } as never);

    expect(vi.mocked(callMcpTool)).toHaveBeenCalledTimes(1);
    expect(vi.mocked(callMcpTool)).toHaveBeenCalledWith("dispatch_pause_status", {
      scope: "project",
    });
  });

  it("fetches global status with exact empty arguments", async () => {
    await fetchGlobalDispatchPauseStatus();

    expect(vi.mocked(callMcpTool)).toHaveBeenCalledWith("dispatch_pause_status", {});
  });

  it("fetches project status with exact scope and target", async () => {
    await fetchProjectDispatchPauseStatus("project-123");

    expect(vi.mocked(callMcpTool)).toHaveBeenCalledWith("dispatch_pause_status", {
      scope: "project",
      target_id: "project-123",
    });
  });

  it("fetches user status with exact scope and target", async () => {
    await fetchUserDispatchPauseStatus("user-123");

    expect(vi.mocked(callMcpTool)).toHaveBeenCalledWith("dispatch_pause_status", {
      scope: "user",
      target_id: "user-123",
    });
  });

  it("never calls dispatch_pause or dispatch_resume from status helpers", async () => {
    await fetchDispatchPauseStatus();
    await fetchGlobalDispatchPauseStatus();
    await fetchProjectDispatchPauseStatus("project-123");
    await fetchUserDispatchPauseStatus("user-123");

    const toolNames = vi.mocked(callMcpTool).mock.calls.map(([toolName]) => toolName);
    expect(toolNames).toEqual([
      "dispatch_pause_status",
      "dispatch_pause_status",
      "dispatch_pause_status",
      "dispatch_pause_status",
    ]);
    expect(toolNames).not.toContain("dispatch_pause");
    expect(toolNames).not.toContain("dispatch_resume");
  });
});
