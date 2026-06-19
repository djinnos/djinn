import { describe, it, expect, beforeEach, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@/test/test-utils";

import { CodeGraphPage } from "@/pages/CodeGraphPage";
import { projectStore } from "@/stores/projectStore";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import type { Project } from "@/api/types";
import type { CodeGraphWorkspace } from "@/api/codeGraph";

// Sigma + WebGL aren't worth wiring up in jsdom; we stub the constructor so
// the smoke test only validates the React surface (project picker shell,
// canvas container, fetch / loading / empty copy).
vi.mock("sigma", () => ({
  default: class MockSigma {
    getCamera() {
      return { animatedReset: () => {} };
    }
    kill() {}
  },
}));

// `@sigma/edge-curve` ships an ES module that does some immediate
// WebGL probing on import; mock it out so jsdom doesn't blow up.
vi.mock("@sigma/edge-curve", () => ({
  default: class MockEdgeCurveProgram {},
}));

// FA2 worker uses Web Workers / shared array buffers under the hood.
// In jsdom the supervisor still constructs but `start()` would emit
// DOMException; mock to a no-op so the lifecycle test doesn't depend
// on a worker runtime.
vi.mock("graphology-layout-forceatlas2/worker", () => ({
  default: class MockSupervisor {
    isRunning() {
      return false;
    }
    start() {}
    stop() {}
    kill() {}
  },
}));

// Default to "no warmed graph" — individual tests can override via
// `mockImplementation` / `mockResolvedValueOnce` to inject populated
// snapshots (iter 30: with cognitive complexity for the heatmap tests).
type SnapshotResponse = { snapshot: Record<string, unknown> };
const fetchSnapshotMock = vi.fn<
  (project: string, nodeCap?: number) => Promise<SnapshotResponse>
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
    fetchSnapshot: (...args: [string, number?]) => fetchSnapshotMock(...args),
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

describe("CodeGraphPage", () => {
  beforeEach(() => {
    localStorage.clear();
    fetchSnapshotMock.mockReset();
    fetchWorkspacesMock.mockReset();
    useCodeGraphStore.getState().reset();
    useCodeGraphStore.getState().setSelectedWorkspaceSlug(null);
    fetchWorkspacesMock.mockResolvedValue([]);
    // Default snapshot fixture: empty graph, no complexity data.
    fetchSnapshotMock.mockImplementation(async () => ({
      snapshot: {
        project_id: "project-a",
        git_head: "deadbeef",
        generated_at: "2026-04-28T00:00:00Z",
        truncated: false,
        total_nodes: 0,
        total_edges: 0,
        node_cap: 2_000,
        nodes: [],
        edges: [],
      },
    }));
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
    expect(screen.queryByTestId("code-graph-canvas")).not.toBeInTheDocument();
  });

  it("mounts the Sigma canvas and fetches a snapshot once a project is selected", async () => {
    projectStore.setState({
      projects: projectsFixture,
      selectedProjectId: "project-a",
      lastViewPerProject: {},
    });

    render(<CodeGraphPage />);

    expect(screen.getByTestId("code-graph-canvas")).toBeInTheDocument();
    expect(screen.getByLabelText(/select project/i)).toBeInTheDocument();
    await waitFor(() => {
      expect(fetchSnapshotMock).toHaveBeenCalledWith("project-a", 10_000);
    });
  });

  it("falls back to the no-projects copy when the project list is empty", () => {
    projectStore.setState({
      projects: [],
      selectedProjectId: null,
      lastViewPerProject: {},
    });

    render(<CodeGraphPage />);

    expect(
      screen.getByText(/no projects yet\. add one from the repositories page\./i),
    ).toBeInTheDocument();
  });

  it("surfaces a friendly empty hint when the snapshot has no nodes", async () => {
    projectStore.setState({
      projects: projectsFixture,
      selectedProjectId: "project-a",
      lastViewPerProject: {},
    });

    render(<CodeGraphPage />);

    await waitFor(() => {
      expect(
        screen.getByText(/no graph data yet/i),
      ).toBeInTheDocument();
    });
  });

  it("fetches workspaces without blocking snapshot rendering and hides the selector for empty, single, or failed lists", async () => {
    fetchWorkspacesMock.mockResolvedValueOnce([]);
    projectStore.setState({
      projects: projectsFixture,
      selectedProjectId: "project-a",
      lastViewPerProject: {},
    });

    const emptyView = render(<CodeGraphPage />);

    expect(screen.getByTestId("code-graph-canvas")).toBeInTheDocument();
    await waitFor(() => {
      expect(fetchWorkspacesMock).toHaveBeenCalledWith("project-a");
    });
    await waitFor(() => {
      expect(screen.queryByLabelText(/select workspace/i)).not.toBeInTheDocument();
    });

    emptyView.unmount();
    fetchWorkspacesMock.mockResolvedValueOnce([{ slug: "api" }]);
    const singleView = render(<CodeGraphPage />);
    await waitFor(() => {
      expect(fetchWorkspacesMock).toHaveBeenCalledTimes(2);
    });
    await waitFor(() => {
      expect(screen.queryByLabelText(/select workspace/i)).not.toBeInTheDocument();
    });

    singleView.unmount();
    fetchWorkspacesMock.mockRejectedValueOnce(new Error("workspace API down"));
    render(<CodeGraphPage />);
    await waitFor(() => {
      expect(screen.getByText(/workspace api down/i)).toBeInTheDocument();
    });
    expect(screen.getByTestId("code-graph-canvas")).toBeInTheDocument();
  });

  it("shows All plus workspace options and stores selected workspace or All", async () => {
    fetchWorkspacesMock.mockResolvedValue([
      { slug: "api", display: "API" },
      { slug: "web", display: "Web" },
    ]);
    projectStore.setState({
      projects: projectsFixture,
      selectedProjectId: "project-a",
      lastViewPerProject: {},
    });

    render(<CodeGraphPage />);

    const selector = await screen.findByLabelText(/select workspace/i);
    expect(screen.getByRole("option", { name: "All" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "API" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "Web" })).toBeInTheDocument();

    fireEvent.change(selector, { target: { value: "api" } });
    expect(useCodeGraphStore.getState().selectedWorkspaceSlug).toBe("api");
    expect(fetchSnapshotMock).toHaveBeenCalledTimes(1);
    expect(fetchSnapshotMock).toHaveBeenLastCalledWith("project-a", 10_000);

    fireEvent.change(selector, { target: { value: "" } });
    expect(useCodeGraphStore.getState().selectedWorkspaceSlug).toBeNull();
    expect(fetchSnapshotMock).toHaveBeenCalledTimes(1);
  });

  it("persists workspace selection across canvas remounts for the same project", async () => {
    fetchWorkspacesMock.mockResolvedValue([
      { slug: "api", display: "API" },
      { slug: "web", display: "Web" },
    ]);
    projectStore.setState({
      projects: projectsFixture,
      selectedProjectId: "project-a",
      lastViewPerProject: {},
    });

    const { rerender } = render(<CodeGraphPage />);

    fireEvent.change(await screen.findByLabelText(/select workspace/i), {
      target: { value: "web" },
    });
    expect(useCodeGraphStore.getState().selectedWorkspaceSlug).toBe("web");

    rerender(<CodeGraphPage />);

    expect(useCodeGraphStore.getState().selectedWorkspaceSlug).toBe("web");
    expect(screen.getByLabelText(/select workspace/i)).toHaveValue("web");
    expect(fetchWorkspacesMock).toHaveBeenCalledTimes(1);
  });

  it("resets workspace selection when the project changes or the slug is no longer valid", async () => {
    fetchWorkspacesMock.mockImplementation(async (project) =>
      project === "project-a"
        ? [
            { slug: "api", display: "API" },
            { slug: "web", display: "Web" },
          ]
        : [
            { slug: "worker", display: "Worker" },
            { slug: "cli", display: "CLI" },
          ],
    );
    projectStore.setState({
      projects: projectsFixture,
      selectedProjectId: "project-a",
      lastViewPerProject: {},
    });

    render(<CodeGraphPage />);

    fireEvent.change(await screen.findByLabelText(/select workspace/i), {
      target: { value: "api" },
    });
    expect(useCodeGraphStore.getState().selectedWorkspaceSlug).toBe("api");

    fireEvent.change(screen.getByLabelText(/select project/i), {
      target: { value: "project-b" },
    });

    await waitFor(() => {
      expect(fetchWorkspacesMock).toHaveBeenCalledWith("project-b");
    });
    expect(useCodeGraphStore.getState().selectedWorkspaceSlug).toBeNull();

    fireEvent.change(await screen.findByLabelText(/select workspace/i), {
      target: { value: "worker" },
    });
    expect(useCodeGraphStore.getState().selectedWorkspaceSlug).toBe("worker");

    fetchWorkspacesMock.mockResolvedValueOnce([
      { slug: "api", display: "API" },
      { slug: "web", display: "Web" },
    ]);
    fireEvent.change(screen.getByLabelText(/select project/i), {
      target: { value: "project-a" },
    });

    await waitFor(() => {
      expect(useCodeGraphStore.getState().selectedWorkspaceSlug).toBeNull();
    });
  });

  // ── Iter 30: complexity heatmap toggle ─────────────────────────────────

  it("renders the color-mode toggle in the toolbar (iter 30)", async () => {
    projectStore.setState({
      projects: projectsFixture,
      selectedProjectId: "project-a",
      lastViewPerProject: {},
    });

    render(<CodeGraphPage />);

    expect(await screen.findByTestId("color-mode-toggle")).toBeInTheDocument();
    expect(screen.getByTestId("color-mode-topology")).toHaveAttribute(
      "aria-checked",
      "true",
    );
    expect(screen.getByTestId("color-mode-complexity")).toHaveAttribute(
      "aria-checked",
      "false",
    );
  });

  it("disables the heatmap option when no nodes carry cognitive (iter 30)", async () => {
    projectStore.setState({
      projects: projectsFixture,
      selectedProjectId: "project-a",
      lastViewPerProject: {},
    });

    render(<CodeGraphPage />);

    await waitFor(() => {
      expect(fetchSnapshotMock).toHaveBeenCalled();
    });
    // The empty fixture has no function nodes, so complexity data is
    // unavailable and the heatmap option must be disabled.
    const complexityBtn = await screen.findByTestId("color-mode-complexity");
    expect(complexityBtn).toBeDisabled();
    expect(complexityBtn).toHaveAttribute(
      "title",
      expect.stringMatching(/no complexity data/i),
    );
  });

  it("enables the heatmap option once a snapshot carries cognitive (iter 30)", async () => {
    fetchSnapshotMock.mockImplementation(async () => ({
      snapshot: {
        project_id: "project-a",
        git_head: "deadbeef",
        generated_at: "2026-04-28T00:00:00Z",
        truncated: false,
        total_nodes: 3,
        total_edges: 0,
        node_cap: 2_000,
        nodes: [
          {
            id: "symbol:fn_a",
            kind: "symbol",
            label: "fn_a",
            symbol_kind: "function",
            file_path: "src/a.rs",
            pagerank: 0.5,
            cognitive: 3,
          },
          {
            id: "symbol:fn_b",
            kind: "symbol",
            label: "fn_b",
            symbol_kind: "function",
            file_path: "src/b.rs",
            pagerank: 0.3,
            cognitive: 12,
          },
          {
            id: "symbol:fn_c",
            kind: "symbol",
            label: "fn_c",
            symbol_kind: "function",
            file_path: "src/c.rs",
            pagerank: 0.2,
            cognitive: 30,
          },
        ],
        edges: [],
      },
    }));

    projectStore.setState({
      projects: projectsFixture,
      selectedProjectId: "project-a",
      lastViewPerProject: {},
    });

    render(<CodeGraphPage />);

    const complexityBtn = await screen.findByTestId("color-mode-complexity");
    await waitFor(() => {
      expect(complexityBtn).not.toBeDisabled();
    });
    expect(useCodeGraphStore.getState().complexityAvailable).toBe(true);
  });

  it("flips the store mode when the toggle is clicked (iter 30)", async () => {
    fetchSnapshotMock.mockImplementation(async () => ({
      snapshot: {
        project_id: "project-a",
        git_head: "deadbeef",
        generated_at: "2026-04-28T00:00:00Z",
        truncated: false,
        total_nodes: 1,
        total_edges: 0,
        node_cap: 2_000,
        nodes: [
          {
            id: "symbol:fn_a",
            kind: "symbol",
            label: "fn_a",
            symbol_kind: "function",
            file_path: "src/a.rs",
            pagerank: 0.5,
            cognitive: 7,
          },
        ],
        edges: [],
      },
    }));

    projectStore.setState({
      projects: projectsFixture,
      selectedProjectId: "project-a",
      lastViewPerProject: {},
    });

    render(<CodeGraphPage />);

    const complexityBtn = await screen.findByTestId("color-mode-complexity");
    await waitFor(() => {
      expect(complexityBtn).not.toBeDisabled();
    });
    fireEvent.click(complexityBtn);
    expect(useCodeGraphStore.getState().colorMode).toBe("complexity");

    // Toggling back to topology works too.
    fireEvent.click(screen.getByTestId("color-mode-topology"));
    expect(useCodeGraphStore.getState().colorMode).toBe("topology");
  });
});
