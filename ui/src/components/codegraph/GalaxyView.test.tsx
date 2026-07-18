import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const fetchArtifact = vi.fn();
const fetchSnapshot = vi.fn();
const layoutInWorker = vi.fn();
const snapshotToGalaxy = vi.fn((snapshot: { project_id: string }) => ({
  nodes: [{ id: snapshot.project_id }],
  edges: [],
  serverPositioned: snapshot.project_id !== "old-project",
}));

vi.mock("@/api/galaxyArtifact", () => ({ fetchGalaxyArtifact: (...args: unknown[]) => fetchArtifact(...args) }));
vi.mock("@/api/codeGraph", () => ({
  fetchCoverage: vi.fn().mockResolvedValue(null),
  fetchSnapshot: (...args: unknown[]) => fetchSnapshot(...args),
  fetchWorkspaces: vi.fn().mockResolvedValue([]),
}));
vi.mock("@/lib/codeGraphAdapter", () => ({ parseSnapshotResponse: (value: unknown) => value }));
vi.mock("@/lib/codeGraphGalaxyAdapter", () => ({
  galaxyLayoutSeed: vi.fn(),
  snapshotHotspots: vi.fn().mockReturnValue([]),
  snapshotToGalaxy: (...args: unknown[]) => snapshotToGalaxy(args[0] as { project_id: string }),
}));
vi.mock("@/components/galaxy/galaxyLayoutClient", () => ({
  layoutInWorker: (...args: unknown[]) => layoutInWorker(...args),
}));
vi.mock("@/components/codegraph/CoverageGapBanner", () => ({ CoverageGapBanner: () => null }));
vi.mock("@/components/codegraph/HotspotPanel", () => ({ HotspotPanel: () => null }));
vi.mock("@/components/galaxy/GalaxyCanvas", () => ({
  GalaxyCanvas: ({ data }: { data: { nodes: Array<{ id: string }> } }) => <div data-testid="galaxy">{data.nodes[0]?.id}</div>,
}));
vi.mock("@/stores/codeGraphStore", () => ({ useCodeGraphStore: () => new Set<string>() }));
vi.mock("@/stores/useProjectStore", () => ({
  useProjects: () => [],
  useProjectStore: (selector: (state: { setSelectedProjectId: () => void }) => unknown) => selector({ setSelectedProjectId: () => {} }),
  useSelectedProject: () => null,
}));

import { GalaxyView } from "./GalaxyView";

function snapshot(project_id: string) {
  return {
    project_id, git_head: "head", generated_at: "now", truncated: false,
    total_nodes: 1, total_edges: 0, node_cap: 10_000,
    nodes: [{ id: project_id, kind: "file", label: project_id, pagerank: 1 }], edges: [],
  };
}

beforeEach(() => {
  fetchArtifact.mockReset();
  fetchSnapshot.mockReset();
  layoutInWorker.mockReset();
  snapshotToGalaxy.mockClear();
});
afterEach(cleanup);

describe("GalaxyView REST artifact cutover", () => {
  it("installs REST artifact data and uses bounded MCP only for explicit fallback", async () => {
    fetchArtifact.mockResolvedValueOnce({ kind: "artifact", artifact: { snapshot: snapshot("rest-project") } });
    render(<GalaxyView projectId="rest-project" />);
    await expect(screen.findByTestId("galaxy")).resolves.toHaveTextContent("rest-project");
    expect(fetchSnapshot).not.toHaveBeenCalled();

    cleanup();
    fetchArtifact.mockResolvedValueOnce({ kind: "fallback", reason: "unavailable" });
    fetchSnapshot.mockResolvedValueOnce(snapshot("fallback-project"));
    render(<GalaxyView projectId="fallback-project" />);
    await expect(screen.findByTestId("galaxy")).resolves.toHaveTextContent("fallback-project");
    expect(fetchSnapshot).toHaveBeenCalledWith("fallback-project", 10_000);
  });

  it("ignores a late MCP fallback from the prior project before parsing or layout", async () => {
    let resolveFallback!: (value: ReturnType<typeof snapshot>) => void;
    fetchArtifact.mockResolvedValueOnce({ kind: "fallback", reason: "unavailable" });
    fetchSnapshot.mockImplementationOnce(
      () => new Promise((resolve) => { resolveFallback = resolve; }),
    );
    fetchArtifact.mockResolvedValueOnce({ kind: "artifact", artifact: { snapshot: snapshot("new-project") } });
    const rendered = render(<GalaxyView projectId="old-project" />);

    await waitFor(() => expect(fetchSnapshot).toHaveBeenCalledWith("old-project", 10_000));
    rendered.rerender(<GalaxyView projectId="new-project" />);
    await waitFor(() => expect(screen.getByTestId("galaxy")).toHaveTextContent("new-project"));
    expect((fetchArtifact.mock.calls[0][1] as { signal: AbortSignal }).signal.aborted).toBe(true);

    await act(async () => resolveFallback(snapshot("old-project")));

    expect(screen.getByTestId("galaxy")).toHaveTextContent("new-project");
    expect(snapshotToGalaxy).not.toHaveBeenCalledWith(expect.objectContaining({ project_id: "old-project" }));
    expect(layoutInWorker).not.toHaveBeenCalled();
  });
});
