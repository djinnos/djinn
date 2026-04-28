import { describe, it, expect, beforeEach, vi } from "vitest";
import { render, screen } from "@/test/test-utils";

import { CodeGraphPage } from "@/pages/CodeGraphPage";
import { projectStore } from "@/stores/projectStore";
import type { Project } from "@/api/types";

// Sigma + WebGL aren't worth wiring up in jsdom; we stub the constructor so
// the smoke test only validates the React surface (project picker shell,
// canvas container, empty-state copy). PR D2 owns the layout/render path.
vi.mock("sigma", () => ({
  default: class MockSigma {
    kill() {}
  },
}));

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

  it("mounts the Sigma canvas and shows the D2 placeholder once a project is selected", () => {
    projectStore.setState({
      projects: projectsFixture,
      selectedProjectId: "project-a",
      lastViewPerProject: {},
    });

    render(<CodeGraphPage />);

    expect(screen.getByTestId("code-graph-canvas")).toBeInTheDocument();
    expect(screen.getByLabelText(/select project/i)).toBeInTheDocument();
    expect(
      screen.getByText(/graph rendering lands in pr d2/i),
    ).toBeInTheDocument();
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
});
