import { describe, it, expect, vi, beforeEach } from "vitest";
import { renderHook, act, waitFor } from "@testing-library/react";

import { useTaskActions } from "./useTaskActions";
import { useExecutionControl } from "./useExecutionControl";
import { useExecutionStatus } from "./useExecutionStatus";
import { callMcpTool } from "@/api/mcpClient";

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: vi.fn(),
}));

describe("useTaskActions", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("transition calls task_transition with correct params", async () => {
    vi.mocked(callMcpTool).mockResolvedValue({});
    const { result } = renderHook(() => useTaskActions());

    await act(async () => {
      await result.current.transition("task-1", "/tmp/project", "start", "because");
    });

    expect(callMcpTool).toHaveBeenCalledWith("task_transition", {
      project: "/tmp/project",
      id: "task-1",
      action: "start",
      reason: "because",
    });
  });

  it("busy toggles and transition surfaces errors", async () => {
    const err = new Error("boom");
    vi.mocked(callMcpTool).mockRejectedValue(err);
    const { result } = renderHook(() => useTaskActions());

    expect(result.current.busy).toBe(false);

    await expect(
      act(async () => {
        await result.current.transition("task-1", "/tmp/project", "pause");
      })
    ).rejects.toThrow("boom");

    expect(result.current.busy).toBe(false);
  });
});

describe("useExecutionControl", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("start/pause/resume invoke correct MCP tools", async () => {
    vi.mocked(callMcpTool).mockResolvedValue({});
    const { result } = renderHook(() => useExecutionControl());

    await act(async () => {
      await result.current.start("/tmp/project");
      await result.current.pause("/tmp/project");
      await result.current.resume("/tmp/project");
    });

    expect(callMcpTool).toHaveBeenNthCalledWith(1, "execution_start", { project: "/tmp/project" });
    expect(callMcpTool).toHaveBeenNthCalledWith(2, "execution_pause", { project: "/tmp/project" });
    expect(callMcpTool).toHaveBeenNthCalledWith(3, "execution_resume", { project: "/tmp/project" });
  });
});

describe("useExecutionStatus", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("returns mapped status shape", async () => {
    vi.mocked(callMcpTool).mockResolvedValue({
      state: "running",
      running_sessions: 2,
      max_sessions: 4,
      capacity: { workers: { active: 2, max: 4 } },
      extra: true,
    });

    const { result } = renderHook(() => useExecutionStatus("/tmp/project"));

    await waitFor(() => expect(result.current.state).toBe("running"));

    expect(callMcpTool).toHaveBeenCalledWith("execution_status", { project: "/tmp/project" });
    expect(result.current.runningSessions).toBe(2);
    expect(result.current.maxSessions).toBe(4);
    expect(result.current.capacity).toEqual({ workers: { active: 2, max: 4 } });
    expect(result.current.raw).toEqual({
      state: "running",
      running_sessions: 2,
      max_sessions: 4,
      capacity: { workers: { active: 2, max: 4 } },
      extra: true,
    });
  });

  it("handles errors by resetting state shape", async () => {
    vi.mocked(callMcpTool).mockRejectedValue(new Error("status failed"));

    const { result } = renderHook(() => useExecutionStatus("/tmp/project"));

    await waitFor(() => {
      expect(result.current.state).toBeNull();
      expect(result.current.runningSessions).toBe(0);
      expect(result.current.maxSessions).toBe(0);
      expect(result.current.capacity).toEqual({});
      expect(result.current.raw).toBeNull();
    });
  });
});
