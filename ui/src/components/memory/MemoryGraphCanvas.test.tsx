import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import {
  buildMemoryGraphDisk, desaturate, ghostNodeRenderSemantics, GHOST_INTERACTION_OPACITY, GHOST_LINK_OPACITY,
  GHOST_OPACITY, isRecentLifecycleTransition, LIFECYCLE_FADE_MS, lifecycleGhostOpacity, memoryGraphCameraFitRadius,
  memoryGraphHitSlop, memoryGraphHoverPillMeta, MemoryGraphCanvas, shouldStartLifecycleFade, visualLinkDirection, visualLinkStyle,
} from "./MemoryGraphCanvas";
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

describe("lifecycle ghost semantics", () => {
  it("exposes and activates a labelled keyboard ghost control", async () => {
    const onSelectNote = vi.fn();
    render(<MemoryGraphCanvas projectSlug="owner/a11y" onSelectNote={onSelectNote} />);
    const ghost = await screen.findByRole("button", { name: "Archived note — archived" });
    fireEvent.focus(ghost);
    fireEvent.click(ghost);
    expect(onSelectNote).toHaveBeenCalledWith("notes/archived-note");
  });

  it("uses bounded one-shot fade timing and reduced motion opacity", () => {
    const now = 1_000_000;
    expect(isRecentLifecycleTransition(now - 7 * 86_400, now)).toBe(true);
    expect(isRecentLifecycleTransition(now - 7 * 86_400 - 1, now)).toBe(false);
    expect(isRecentLifecycleTransition(null, now)).toBe(false);
    expect(lifecycleGhostOpacity(true, 10, 10, false)).toBe(GHOST_INTERACTION_OPACITY);
    expect(lifecycleGhostOpacity(true, 10, 610, false)).toBe(GHOST_OPACITY);
    expect(lifecycleGhostOpacity(true, 10, 10, true)).toBe(GHOST_OPACITY);
  });

  it("renders ghost fill, labels, and hit area only in their intended interaction states", () => {
    expect(desaturate("#ff0000")).toBe("rgb(76,76,76)");
    expect(ghostNodeRenderSemantics("#ff0000", true, false, false, undefined, 0, false)).toStrictEqual({
      fill: "rgb(76,76,76)", labelVisible: false, opacity: GHOST_OPACITY,
    });
    // Pointer hover and keyboard focus both feed the same `hot` canvas state.
    expect(ghostNodeRenderSemantics("#ff0000", true, true, false, undefined, 0, false)).toStrictEqual({
      fill: "rgb(76,76,76)", labelVisible: true, opacity: GHOST_INTERACTION_OPACITY,
    });
    expect(memoryGraphHitSlop(1)).toBe(10);
    expect(memoryGraphHitSlop(3)).toBe(6);
    expect(memoryGraphHoverPillMeta({ isGhost: true, isOrphan: false, lifecycle: "archived", noteType: "reference", ts: null })).toBe("reference · archived");
    // Active pills retain the pre-lifecycle metadata format.
    expect(memoryGraphHoverPillMeta({ isGhost: false, isOrphan: false, lifecycle: "active", noteType: "adr", ts: null })).toBe("adr");
  });

  it("keeps quiet ordinary links bright while ghost-connected links are separate and subdued", () => {
    const disk = buildMemoryGraphDisk({
      nodes: [layoutNode("active-a", "2024-01-01T00:00:00Z", "active"), layoutNode("active-b", "2024-01-02T00:00:00Z", "active"), layoutNode("ghost", "2024-01-03T00:00:00Z", "archived")],
      edges: [{ raw_text: "ordinary", source_id: "active-a", target_id: "active-b" }, { raw_text: "ghost", source_id: "active-a", target_id: "ghost" }],
      typed_edges: [
        { source_id: "active-a", target_id: "active-b", kind: "contradicts", weight: 1 },
        { source_id: "active-a", target_id: "ghost", kind: "contradicts", weight: 1 },
      ],
    });
    const styles = disk.links.map((link) => visualLinkStyle(link, disk.nodes));
    expect(styles).toEqual(expect.arrayContaining([
      expect.objectContaining({ batchKey: "wikilink:ordinary", ghostConnected: false, opacity: 0.14 }),
      expect.objectContaining({ batchKey: "wikilink:ghost", ghostConnected: true, opacity: GHOST_LINK_OPACITY }),
      expect.objectContaining({ batchKey: "contradicts:ordinary", dashed: true, ghostConnected: false, opacity: 0.3 }),
      expect.objectContaining({ batchKey: "contradicts:ghost", dashed: true, ghostConnected: true, opacity: GHOST_LINK_OPACITY }),
    ]));
    // Ghost and active-only siblings never share a quiet canvas batch.
    expect(new Set(styles.map((style) => style.batchKey)).size).toBe(4);
  });

  it("directs either canonical mixed supersedes order active-to-ghost and leaves same-class supersedes undirected", () => {
    for (const [source, target] of [["active", "ghost"], ["ghost", "active"]]) {
      const disk = buildMemoryGraphDisk({ nodes: [layoutNode("active", "2024-01-01T00:00:00Z", "active"), layoutNode("ghost", "2024-01-02T00:00:00Z", "archived")], edges: [], typed_edges: [{ source_id: source, target_id: target, kind: "supersedes", weight: 1 }] });
      const direction = visualLinkDirection(disk.links[0], disk.nodes);
      expect([disk.nodes[direction.from].id, disk.nodes[direction.to].id, direction.directed]).toStrictEqual(["active", "ghost", true]);
    }
    const sameClass = buildMemoryGraphDisk({ nodes: [layoutNode("first", "2024-01-01T00:00:00Z", "active"), layoutNode("second", "2024-01-02T00:00:00Z", "active")], edges: [], typed_edges: [{ source_id: "first", target_id: "second", kind: "supersedes", weight: 1 }] });
    expect(visualLinkDirection(sameClass.links[0], sameClass.nodes).directed).toBe(false);
  });

  it("keeps ghost-connected contradicts non-directional and dashed", () => {
    const disk = buildMemoryGraphDisk({ nodes: [layoutNode("active", "2024-01-01T00:00:00Z", "active"), layoutNode("ghost", "2024-01-02T00:00:00Z", "deprecated")], edges: [], typed_edges: [{ source_id: "ghost", target_id: "active", kind: "contradicts", weight: 1 }] });
    expect(visualLinkDirection(disk.links[0], disk.nodes).directed).toBe(false);
    expect(visualLinkStyle(disk.links[0], disk.nodes)).toMatchObject({ dashed: true, opacity: GHOST_LINK_OPACITY });
  });

  it("does not restart a completed recent fade across hover or rerender state", () => {
    const now = 1_000_000;
    const completed = new Set<string>();
    expect(shouldStartLifecycleFade(true, false, 1, undefined, completed, "ghost")).toBe(true);
    expect(lifecycleGhostOpacity(true, now, now + LIFECYCLE_FADE_MS, false)).toBe(GHOST_OPACITY);
    completed.add("ghost"); // Render completion persists in the component ref.
    expect(shouldStartLifecycleFade(true, false, 1, undefined, completed, "ghost")).toBe(false);
    expect(shouldStartLifecycleFade(true, false, 1, undefined, completed, "ghost")).toBe(false);
    expect(shouldStartLifecycleFade(true, true, 1, undefined, new Set(), "ghost")).toBe(false);
  });
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
