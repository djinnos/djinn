import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { MemoryGraphCanvas } from "./MemoryGraphCanvas";
import { validLifecycleResponse } from "@/lib/__fixtures__/memoryGraphLifecycle";

const callMcpToolMock = vi.fn();

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: (...args: unknown[]) => callMcpToolMock(...args),
}));

const activeOnlyResponse = {
  ...validLifecycleResponse,
  nodes: validLifecycleResponse.nodes.filter((node) => node.status === "active"),
  edges: [],
  typed_edges: [],
};

beforeEach(() => {
  window.localStorage.clear();
  callMcpToolMock.mockReset();
  callMcpToolMock.mockResolvedValue(validLifecycleResponse);
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(null);
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

async function expectGhostFetch(project: string) {
  await waitFor(() => {
    expect(callMcpToolMock).toHaveBeenLastCalledWith("memory_graph", {
      project,
      statuses: ["active", "archived", "deprecated"],
      lifecycle_limit: 500,
    });
  });
}

describe("MemoryGraphCanvas lifecycle ghost preference", () => {
  it("defaults on and requests the lifecycle payload", async () => {
    render(<MemoryGraphCanvas projectSlug="owner/default" />);

    expect(screen.getByLabelText("Show lifecycle ghosts")).toBeChecked();
    await expectGhostFetch("owner/default");
  });

  it("persists exact project-scoped values and refetches active-only when disabled", async () => {
    render(<MemoryGraphCanvas projectSlug="owner/one" />);
    await expectGhostFetch("owner/one");

    fireEvent.click(screen.getByLabelText("Show lifecycle ghosts"));
    expect(window.localStorage.getItem("djinn:memory-graph:lifecycle-ghosts:owner/one")).toBe("0");
    await waitFor(() => {
      expect(callMcpToolMock).toHaveBeenLastCalledWith("memory_graph", { project: "owner/one" });
    });

    fireEvent.click(screen.getByLabelText("Show lifecycle ghosts"));
    expect(window.localStorage.getItem("djinn:memory-graph:lifecycle-ghosts:owner/one")).toBe("1");
    await expectGhostFetch("owner/one");
  });

  it("re-reads each project's independent preference before fetching", async () => {
    window.localStorage.setItem("djinn:memory-graph:lifecycle-ghosts:owner/two", "0");
    const { rerender } = render(<MemoryGraphCanvas projectSlug="owner/one" />);
    await expectGhostFetch("owner/one");

    rerender(<MemoryGraphCanvas projectSlug="owner/two" />);
    await waitFor(() => {
      expect(callMcpToolMock).toHaveBeenLastCalledWith("memory_graph", { project: "owner/two" });
    });
    expect(screen.getByLabelText("Show lifecycle ghosts")).not.toBeChecked();
  });

  it("fails open when storage cannot be read or written", async () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new Error("blocked"); });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("blocked"); });
    render(<MemoryGraphCanvas projectSlug="owner/blocked" />);

    await expectGhostFetch("owner/blocked");
    fireEvent.click(screen.getByLabelText("Show lifecycle ghosts"));
    expect(screen.getByLabelText("Show lifecycle ghosts")).toBeChecked();
    await expectGhostFetch("owner/blocked");
  });

  it("replaces lifecycle payload with active-only data and only shows the cap badge while enabled", async () => {
    callMcpToolMock
      .mockResolvedValueOnce({
        ...validLifecycleResponse,
        lifecycle_summary: { inactive_total: 503, inactive_returned: 500, inactive_omitted: 3 },
      })
      .mockResolvedValueOnce(activeOnlyResponse);
    render(<MemoryGraphCanvas projectSlug="owner/cap" />);

    expect(await screen.findByText("500 shown · 3 older hidden")).toBeInTheDocument();
    fireEvent.click(screen.getByLabelText("Show lifecycle ghosts"));
    await waitFor(() => expect(screen.queryByText("500 shown · 3 older hidden")).not.toBeInTheDocument());
    expect(callMcpToolMock).toHaveBeenLastCalledWith("memory_graph", { project: "owner/cap" });
  });
});
