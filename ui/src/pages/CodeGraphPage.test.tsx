import { describe, it, expect, beforeEach, vi } from "vitest";
import { render, screen, waitFor } from "@/test/test-utils";

import { CodeGraphPage } from "@/pages/CodeGraphPage";
import { projectStore } from "@/stores/projectStore";
import type { Project } from "@/api/types";

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
// the wrapper's mock factory below.
const fetchSnapshotMock = vi.fn(async () => ({
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

vi.mock("@/api/codeGraph", async () => {
  const actual = await vi.importActual<typeof import("@/api/codeGraph")>(
    "@/api/codeGraph",
  );
  return {
    ...actual,
    fetchSnapshot: (...args: unknown[]) =>
      fetchSnapshotMock(...(args as [string, number?])),
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
    fetchSnapshotMock.mockClear();
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
});
