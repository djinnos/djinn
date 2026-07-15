import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@/test/test-utils";

import { CodeGraphPage } from "@/pages/CodeGraphPage";
import { projectStore } from "@/stores/projectStore";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import type { Project } from "@/api/types";
import type { CodeGraphWorkspace } from "@/api/codeGraph";

// lmkv cutover: the galaxy (react-three-fiber) IS the code graph. WebGL
// isn't worth wiring up in jsdom — the scene falls back to the
// SceneErrorBoundary while the HUD (stats, pickers, toggles) renders
// normally, so these tests validate the React surface: fetch → adapt →
// layout (synchronous fallback; no Worker in jsdom) → HUD.

// Default to "no warmed graph" — individual tests override via
// `mockResolvedValueOnce` to inject populated snapshots.
type SnapshotResponse = { snapshot: Record<string, unknown> };
const fetchSnapshotMock = vi.fn<
  (project: string, nodeCap?: number, level?: string) => Promise<SnapshotResponse>
>();
const fetchWorkspacesMock = vi.fn<
  (project: string) => Promise<CodeGraphWorkspace[]>
>();

vi.mock("@/api/codeGraph", async () => {
  const actual = await vi.importActual<typeof import("@/api/codeGraph")>(
    "@/api/codeGraph",
  );
  return {
    ...actual,
    fetchSnapshot: (...args: [string, number?, string?]) => fetchSnapshotMock(...args),
    fetchWorkspaces: (project: string) => fetchWorkspacesMock(project),
  };
});

const projectsFixture: Project[] = [
  {
    id: "project-a",
    name: "Project Alpha",
    github_owner: "acme",
    github_repo: "alpha",
  },
  {
    id: "project-b",
    name: "Project Beta",
    github_owner: "acme",
    github_repo: "beta",
  },
];

function snapshotFixture(
  nodes: Array<Record<string, unknown>>,
  edges: Array<Record<string, unknown>> = [],
): SnapshotResponse {
  return {
    snapshot: {
      project_id: "project-a",
      git_head: "deadbeef",
      generated_at: "2026-07-14T00:00:00Z",
      truncated: false,
      total_nodes: nodes.length,
      total_edges: edges.length,
      node_cap: 1_000_000,
      nodes,
      edges,
    },
  };
}

const POPULATED = snapshotFixture(
  [
    {
      id: "file:core/src/lib.rs",
      kind: "file",
      label: "server/crates/djinn-core/src/lib.rs",
      file_path: "server/crates/djinn-core/src/lib.rs",
      pagerank: 0.4,
      workspace: "server",
    },
    {
      id: "sym:core::resolve",
      kind: "symbol",
      label: "resolve",
      symbol_kind: "function",
      file_path: "server/crates/djinn-core/src/lib.rs",
      pagerank: 0.3,
      cognitive: 12,
      workspace: "server",
    },
    {
      id: "file:core/src/lib_test.rs",
      kind: "file",
      label: "server/crates/djinn-core/src/lib_test.rs",
      file_path: "server/crates/djinn-core/src/lib_test.rs",
      pagerank: 0.1,
      is_test: true,
      workspace: "server",
    },
  ],
  [
    {
      from: "file:core/src/lib.rs",
      to: "sym:core::resolve",
      kind: "ContainsDefinition",
      confidence: 1,
    },
  ],
);

function selectProjectA() {
  projectStore.setState({
    projects: projectsFixture,
    selectedProjectId: "project-a",
    lastViewPerProject: {},
  });
}

describe("CodeGraphPage (galaxy)", () => {
  beforeEach(() => {
    localStorage?.clear?.();
    fetchSnapshotMock.mockReset();
    fetchWorkspacesMock.mockReset();
    useCodeGraphStore.getState().reset();
    fetchWorkspacesMock.mockResolvedValue([]);
    fetchSnapshotMock.mockImplementation(async () => snapshotFixture([]));
  });

  it("renders the empty-state hint when no project is selected", () => {
    projectStore.setState({
      projects: projectsFixture,
      selectedProjectId: null,
      lastViewPerProject: {},
    });

    render(<CodeGraphPage />);

    expect(
      screen.getByText(/select a project to view its code graph/i),
    ).toBeInTheDocument();
    expect(screen.queryByTestId("galaxy-canvas")).not.toBeInTheDocument();
  });

  it("fetches the snapshot and renders the galaxy HUD once a project is selected", async () => {
    fetchSnapshotMock.mockResolvedValueOnce(POPULATED);
    selectProjectA();

    render(<CodeGraphPage />);

    await waitFor(() => {
      expect(screen.getByTestId("galaxy-canvas")).toBeInTheDocument();
    });
    // Stats line: 3 nodes / 1 edges.
    expect(screen.getByText(/3 nodes/)).toBeInTheDocument();
    expect(screen.getByText(/1 edges/)).toBeInTheDocument();
    // Project chip replaces the old page-local pickers.
    expect(screen.getByText("Project Alpha")).toBeInTheDocument();
    // Whole-graph budget is requested — the server clamps as it sees fit.
    expect(fetchSnapshotMock).toHaveBeenCalledWith("project-a", 1_000_000);
  });

  it("surfaces the snapshot-unavailable message when the fetch fails", async () => {
    fetchSnapshotMock.mockRejectedValueOnce(new Error("graph not warmed"));
    selectProjectA();

    render(<CodeGraphPage />);

    await waitFor(() => {
      expect(screen.getByText(/graph not warmed/i)).toBeInTheDocument();
    });
  });

  it("hides tests via the hide-tests toggle without refetching", async () => {
    fetchSnapshotMock.mockResolvedValueOnce(POPULATED);
    selectProjectA();

    render(<CodeGraphPage />);

    await waitFor(() => {
      expect(screen.getByText(/3 nodes/)).toBeInTheDocument();
    });

    fireEvent.click(screen.getByRole("button", { name: /hide tests/i }));

    await waitFor(() => {
      expect(screen.getByText(/2 nodes/)).toBeInTheDocument();
    });
    expect(screen.getByRole("button", { name: /tests hidden/i })).toBeInTheDocument();
    expect(fetchSnapshotMock).toHaveBeenCalledTimes(1);
  });

  it("renders the workspace picker only when multiple workspaces exist", async () => {
    fetchSnapshotMock.mockResolvedValueOnce(POPULATED);
    fetchWorkspacesMock.mockResolvedValue([
      { slug: "server", display: "server" },
      { slug: "ui", display: "ui" },
    ] as CodeGraphWorkspace[]);
    selectProjectA();

    render(<CodeGraphPage />);

    await waitFor(() => {
      expect(screen.getByTestId("galaxy-canvas")).toBeInTheDocument();
    });
    await waitFor(() => {
      expect(
        screen.getByRole("combobox", { name: /workspace/i }),
      ).toBeInTheDocument();
    });
  });

  it("omits the workspace picker for single-workspace projects", async () => {
    fetchSnapshotMock.mockResolvedValueOnce(POPULATED);
    fetchWorkspacesMock.mockResolvedValue([
      { slug: "server", display: "server" },
    ] as CodeGraphWorkspace[]);
    selectProjectA();

    render(<CodeGraphPage />);

    await waitFor(() => {
      expect(screen.getByTestId("galaxy-canvas")).toBeInTheDocument();
    });
    expect(
      screen.queryByRole("combobox", { name: /workspace/i }),
    ).not.toBeInTheDocument();
  });

  it("offers the crate/complexity color modes", async () => {
    fetchSnapshotMock.mockResolvedValueOnce(POPULATED);
    selectProjectA();

    render(<CodeGraphPage />);

    await waitFor(() => {
      expect(
        screen.getByRole("combobox", { name: /color mode/i }),
      ).toBeInTheDocument();
    });
  });

  it("does NOT render a local project picker in the page body (shared chrome handles it)", () => {
    selectProjectA();

    render(<CodeGraphPage />);

    expect(screen.queryByLabelText(/^project$/i)).not.toBeInTheDocument();
  });
});
