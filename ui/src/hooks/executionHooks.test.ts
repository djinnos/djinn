import { describe, it, expect, vi, beforeEach } from "vitest";
import { renderHook, act } from "@testing-library/react";

import { useTaskActions } from "./useTaskActions";
import { useExecutionControl } from "./useExecutionControl";
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

  it("killTask invokes execution_kill_task", async () => {
    vi.mocked(callMcpTool).mockResolvedValue({});
    const { result } = renderHook(() => useExecutionControl());

    await act(async () => {
      await result.current.killTask("task-42");
    });

    expect(callMcpTool).toHaveBeenCalledWith("execution_kill_task", { task_id: "task-42" });
  });
});
