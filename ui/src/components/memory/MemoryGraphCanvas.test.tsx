import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { buildMemoryGraphDisk, MemoryGraphCanvas, memoryGraphCameraFitRadius } from "./MemoryGraphCanvas";
import type { MemoryGraphOutput } from "@/api/generated/mcp-tools.gen";
import { validLifecycleResponse } from "@/lib/__fixtures__/memoryGraphLifecycle";

const { callMcpToolMock } = vi.hoisted(() => ({
  callMcpToolMock: vi.fn(),
}));

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
    expect(screen.getByText("reference")).toBeInTheDocument();
    expect(screen.getByText("pitfall")).toBeInTheDocument();
    fireEvent.click(screen.getByLabelText("Show lifecycle ghosts"));
    await waitFor(() => {
      expect(callMcpToolMock).toHaveBeenLastCalledWith("memory_graph", { project: "owner/cap" });
      expect(screen.queryByText("500 shown · 3 older hidden")).not.toBeInTheDocument();
      // These lifecycle-only legend entries derive from the current disk. Their
      // disappearance proves the active-only response replaced, rather than
      // merely hid, the prior lifecycle-inclusive disk and its hit targets.
      expect(screen.queryByText("reference")).not.toBeInTheDocument();
      expect(screen.queryByText("pitfall")).not.toBeInTheDocument();
      expect(screen.getByText("adr")).toBeInTheDocument();
    });
  });
});

const layoutNode = (
  id: string,
  created_at: string,
  status?: "active" | "archived" | "deprecated",
  connection_count = 0,
): MemoryGraphOutput["nodes"][number] => ({
  connection_count,
  created_at,
  folder: "notes",
  id,
  note_type: "adr",
  permalink: `notes/${id}`,
  ...(status ? { status } : {}),
  title: id,
});

const activeLayoutPayload = (): MemoryGraphOutput => ({
  nodes: [
    layoutNode("active-old", "2024-01-01T00:00:00Z", "active", 1),
    layoutNode("active-middle", "2024-02-01T00:00:00Z", "active", 4),
    layoutNode("proposal-new", "2024-03-01T00:00:00Z", "active", 2),
  ],
  edges: [
    { raw_text: "middle", source_id: "active-old", target_id: "active-middle" },
    { raw_text: "proposal", source_id: "active-middle", target_id: "proposal-new" },
  ],
  typed_edges: [],
});

const activeFields = (payload: MemoryGraphOutput) => {
  const disk = buildMemoryGraphDisk(payload);
  return {
    cameraFitRadius: memoryGraphCameraFitRadius(disk),
    nodes: disk.nodes.filter((node) => !node.isGhost).map(({ id, igniteAt, r, rec, ring, tr, x, y }) =>
      ({ id, igniteAt, r, rec, ring, tr, x, y })).sort((a, b) => a.id.localeCompare(b.id)),
  };
};

describe("buildMemoryGraphDisk lifecycle placement", () => {
  it("keeps active coordinates, radii, recency, rings, reveal, and camera fit byte-for-byte", () => {
    const active = activeLayoutPayload();
    const inclusive: MemoryGraphOutput = {
      ...active,
      nodes: [
        ...active.nodes,
        layoutNode("archived-neighbor", "2024-02-15T00:00:00Z", "archived", 99),
        layoutNode("deprecated-fallback", "2024-01-15T00:00:00Z", "deprecated", 88),
      ],
      edges: [...active.edges, { raw_text: "ghost", source_id: "active-middle", target_id: "archived-neighbor" }],
    };
    expect(activeFields(inclusive)).toStrictEqual(activeFields(active));
  });

  it("anchors a linked ghost after active relaxation and repeats its coordinates", () => {
    const active = activeLayoutPayload();
    const payload: MemoryGraphOutput = {
      ...active,
      nodes: [...active.nodes, layoutNode("archived-neighbor", "2024-02-15T00:00:00Z", "archived", 3)],
      edges: [...active.edges, { raw_text: "ghost", source_id: "archived-neighbor", target_id: "active-middle" }],
    };
    const first = buildMemoryGraphDisk(payload);
    const ghost = first.nodes.find((node) => node.id === "archived-neighbor")!;
    const anchor = first.nodes.find((node) => node.id === "active-middle")!;
    const repeated = buildMemoryGraphDisk(payload).nodes.find((node) => node.id === ghost.id!);
    expect(ghost).toMatchObject({ isGhost: true, ring: anchor.ring });
    expect(Math.hypot(ghost.x - anchor.x, ghost.y - anchor.y)).toBeCloseTo(anchor.r + ghost.r + 14, 12);
    expect({ x: repeated!.x, y: repeated!.y }).toStrictEqual({ x: ghost.x, y: ghost.y });
  });

  it("uses the creation-time ring for an unlinked ghost", () => {
    const active = activeLayoutPayload();
    const disk = buildMemoryGraphDisk({
      ...active,
      nodes: [...active.nodes, layoutNode("archived-fallback", "2024-01-15T00:00:00Z", "archived")],
    });
    const ghost = disk.nodes.find((node) => node.id === "archived-fallback")!;
    expect(ghost).toMatchObject({ isGhost: true, ring: disk.nodes.find((node) => node.id === "active-old")!.ring });
  });

  it("treats omitted status as active and inactive-only payloads as ghosts", () => {
    const legacy: MemoryGraphOutput = { nodes: [layoutNode("legacy", "2024-01-01T00:00:00Z")], edges: [], typed_edges: [] };
    const inactiveOnly: MemoryGraphOutput = { nodes: [layoutNode("archived-only", "2024-01-01T00:00:00Z", "archived")], edges: [], typed_edges: [] };
    expect(buildMemoryGraphDisk(legacy).nodes[0]).toMatchObject({ isGhost: false, lifecycle: "active" });
    expect(buildMemoryGraphDisk(inactiveOnly).nodes[0]).toMatchObject({ isGhost: true, lifecycle: "archived" });
  });
});
