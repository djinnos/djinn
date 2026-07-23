/**
 * Environment/ProjectEnvironmentPage — the routed `/projects/:id/environment`
 * per-project config page.
 *
 * The page reads the project name from the real `projectStore` (seeded here)
 * and loads its config imperatively via `fetchEnvironmentConfig` →
 * `callMcpTool("project_environment_config_get")`. The header's
 * `ProjectImagePicker` separately lists the catalog through
 * `callMcpTool("image_list")` and pre-selects `selected_image_id`. A per-tool
 * responder on the aliased `@/api/mcpClient` mock serves both; the page uses
 * `useParams`, so it is mounted under a matching `Routes`/`MemoryRouter`.
 */

import { useEffect } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter, Route, Routes } from "react-router-dom";

import { ProjectEnvironmentPage } from "./ProjectEnvironmentPage";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";
import { projectStore } from "@/stores/projectStore";
import type { Project } from "@/api/types";

// ── Fixtures ─────────────────────────────────────────────────────────────────

const project: Project = {
  id: "project-djinn",
  name: "djinnos/djinn",
  github_owner: "djinnos",
  github_repo: "djinn",
  branch: "main",
};

const catalogImages = [
  {
    id: "img-rust-node",
    name: "Rust + Node",
    description: "Primary monorepo image.",
    status: "ready",
    config: { schema_version: 1, source: "user-edited", languages: {}, workspaces: [], system_packages: [], env: {}, lifecycle: {} },
    service_presets: ["postgres-16"],
  },
  {
    id: "img-python",
    name: "Python 3.13",
    description: "Scripting image.",
    status: "ready",
    config: { schema_version: 1, source: "user-edited", languages: {}, workspaces: [], system_packages: [], env: {}, lifecycle: {} },
    service_presets: [],
  },
];

function envConfig(workspaces: Array<{ root: string; language: string }>): Record<string, unknown> {
  return {
    schema_version: 1,
    source: "user-edited",
    languages: {},
    workspaces,
    system_packages: [],
    env: {},
    lifecycle: {},
  };
}

function makeResponder(workspaces: Array<{ root: string; language: string }>) {
  return (name: string): unknown => {
    switch (name) {
      case "project_environment_config_get":
        return {
          status: "ok",
          config: envConfig(workspaces),
          selected_image_id: "img-rust-node",
          selected_image_name: "Rust + Node",
        };
      case "image_list":
        return { status: "ok", images: catalogImages };
      default:
        return {};
    }
  };
}

// ── Harness ──────────────────────────────────────────────────────────────────

function ProjectSeeder({ children }: { children: React.ReactNode }) {
  useEffect(() => {
    projectStore.setState({
      projects: [project],
      selectedProjectId: project.id,
      lastViewPerProject: {},
    });
    return () => {
      projectStore.setState({ projects: [], selectedProjectId: null, lastViewPerProject: {} });
    };
  }, []);
  return <>{children}</>;
}

function EnvironmentStory() {
  return (
    <MemoryRouter initialEntries={[`/projects/${project.id}/environment`]}>
      <ProjectSeeder>
        <div className="h-screen bg-background text-foreground">
          <Routes>
            <Route
              path="/projects/:id/environment"
              element={<ProjectEnvironmentPage />}
            />
            <Route path="/repositories" element={<div />} />
          </Routes>
        </div>
      </ProjectSeeder>
    </MemoryRouter>
  );
}

const meta = {
  title: "Environment/ProjectEnvironmentPage",
  component: EnvironmentStory,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof EnvironmentStory>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * Two code-graph workspaces configured (a Rust server root, a Node UI root),
 * with the "Rust + Node" catalog image pre-selected in the header picker.
 */
export const Populated: Story = {
  beforeEach: () =>
    setMcpToolResponder(
      makeResponder([
        { root: "server", language: "rust" },
        { root: "ui", language: "node" },
      ]),
    ),
};

/**
 * No workspaces yet → the dashed empty-state prompt inside the workspaces
 * editor, with the catalog image still assigned.
 */
export const NoWorkspaces: Story = {
  beforeEach: () => setMcpToolResponder(makeResponder([])),
};
