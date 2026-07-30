import { Route, Routes } from "react-router-dom";

import { render, screen, userEvent } from "@/test/test-utils";
import { projectStore } from "@/stores/projectStore";
import type { Project } from "@/api/server";
import { RepositoriesPage } from "./RepositoriesPage";

vi.mock("@/components/AuthGate", () => ({
  useAuthUser: () => ({ isAdmin: false }),
}));

vi.mock("@/hooks/useProjectEnvironmentConfig", () => ({
  useProjectEnvironmentConfig: () => ({ data: undefined }),
}));

vi.mock("@/components/AddProjectFromGithubDialog", () => ({
  AddProjectFromGithubDialog: () => null,
}));

vi.mock("@/components/ImageStatusBadge", () => ({
  ImageStatusBadge: () => null,
}));

vi.mock("@/components/images/ProjectImagePicker", () => ({
  ProjectImagePicker: () => null,
}));

vi.mock("@/components/RepositoriesSectionTabs", () => ({
  RepositoriesSectionTabs: () => null,
}));

vi.mock("@/api/server", () => ({
  fetchProjectBranches: vi.fn(),
  fetchProjects: vi.fn(),
  removeProject: vi.fn(),
  updateProject: vi.fn(),
}));

const projects: Project[] = [
  {
    id: "project-readiness",
    name: "djinnos/readiness-fixture",
    github_owner: "djinnos",
    github_repo: "readiness-fixture",
  },
];

describe("RepositoriesPage readiness navigation", () => {
  beforeEach(() => {
    projectStore.setState({
      projects,
      selectedProjectId: projects[0].id,
      lastViewPerProject: {},
    });
  });

  afterEach(() => {
    projectStore.setState({
      projects: [],
      selectedProjectId: null,
      lastViewPerProject: {},
    });
  });

  it("renders the labeled readiness action and navigates to its project route", async () => {
    const user = userEvent.setup();

    render(
      <Routes>
        <Route path="/repositories" element={<RepositoriesPage />} />
        <Route
          path="/projects/:id/readiness"
          element={<div>Project readiness destination</div>}
        />
      </Routes>,
      { wrapperOptions: { routerProps: { initialEntries: ["/repositories"] } } },
    );

    const readiness = screen.getByRole("button", {
      name: "View readiness for djinnos/readiness-fixture",
    });
    expect(readiness).toHaveAttribute("title", "View project readiness");

    await user.click(readiness);

    expect(screen.getByText("Project readiness destination")).toBeInTheDocument();
  });
});
