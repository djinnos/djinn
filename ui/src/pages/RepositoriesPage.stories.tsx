/**
 * Repositories/RepositoriesPage — the routed `/repositories` table.
 *
 * The page reads its rows from the real `projectStore` (seeded here) and each
 * row drives two independent MCP-backed sub-widgets:
 *   - `BranchPicker` fetches `['project', id, 'branches']` via
 *     `fetchProjectBranches` → `callMcpTool("project_branches")`.
 *   - `ImageStatusBadge` polls `callMcpTool("get_project_devcontainer_status")`.
 * Neither goes through a cache seam we can seed by key alone (the branch query
 * is created inside the row with a live `queryFn`), so we install a per-tool
 * responder on the aliased `@/api/mcpClient` mock and branch on the requested
 * `project` id to give each row a distinct status (ready / building / needs
 * image). A per-story `QueryClient` (retry:false) wraps the page.
 */

import { useEffect } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { RepositoriesPage } from "./RepositoriesPage";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";
import { projectStore } from "@/stores/projectStore";
import type { Project } from "@/api/types";

// ── Fixtures ─────────────────────────────────────────────────────────────────

const projects: Project[] = [
  {
    id: "project-djinn",
    name: "djinnos/djinn",
    github_owner: "djinnos",
    github_repo: "djinn",
    branch: "main",
  },
  {
    id: "project-catalog",
    name: "djinnos/catalog",
    github_owner: "djinnos",
    github_repo: "catalog",
    branch: "release/2026.07",
  },
  {
    id: "project-web",
    name: "djinnos/pink-web",
    github_owner: "djinnos",
    github_repo: "pink-web",
    branch: "main",
  },
];

const branchesByProject: Record<string, string[]> = {
  "project-djinn": ["main", "develop", "feat/graph-warm", "hotfix/oom"],
  "project-catalog": ["main", "release/2026.07", "release/2026.06"],
  "project-web": ["main", "redesign", "wip/pricing"],
};

// A distinct image/warm status per project so the row badges read differently:
// djinn is healthy (no badge), catalog is mid-build, pink-web needs an image.
function devcontainerStatusFor(projectId: string): unknown {
  switch (projectId) {
    case "project-catalog":
      return { image_status: "building", graph_warm_status: "pending", needs_image: false };
    case "project-web":
      return { image_status: "none", graph_warm_status: "pending", needs_image: true };
    default:
      return { image_status: "ready", graph_warm_status: "ready", needs_image: false };
  }
}

function repositoriesResponder(
  name: string,
  args: Record<string, unknown> | undefined,
): unknown {
  switch (name) {
    case "project_branches": {
      const id = String(args?.project_id ?? "");
      return { status: "ok", branches: branchesByProject[id] ?? ["main"], current: null };
    }
    case "get_project_devcontainer_status":
      return devcontainerStatusFor(String(args?.project ?? ""));
    default:
      return {};
  }
}

// ── Harness ──────────────────────────────────────────────────────────────────

function ProjectSeeder({
  seeded,
  children,
}: {
  seeded: Project[];
  children: React.ReactNode;
}) {
  useEffect(() => {
    projectStore.setState({
      projects: seeded,
      selectedProjectId: seeded[0]?.id ?? null,
      lastViewPerProject: {},
    });
    return () => {
      projectStore.setState({ projects: [], selectedProjectId: null, lastViewPerProject: {} });
    };
  }, [seeded]);
  return <>{children}</>;
}

interface RepositoriesStoryArgs {
  seeded: Project[];
}

function RepositoriesStory({ seeded }: RepositoriesStoryArgs) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  return (
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={["/repositories"]}>
        <ProjectSeeder seeded={seeded}>
          <div className="h-screen bg-background text-foreground">
            <Routes>
              <Route path="/repositories" element={<RepositoriesPage />} />
              {/* Sinks for row / action navigation. */}
              <Route path="/tasks" element={<div />} />
              <Route path="/projects/:id/environment" element={<div />} />
            </Routes>
          </div>
        </ProjectSeeder>
      </MemoryRouter>
    </QueryClientProvider>
  );
}

const meta = {
  title: "Repositories/RepositoriesPage",
  component: RepositoriesStory,
  parameters: { layout: "fullscreen" },
  beforeEach: () => {
    setMcpToolResponder(repositoriesResponder);
  },
} satisfies Meta<typeof RepositoriesStory>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * Three repositories, each with a populated branch picker and a distinct image
 * state: djinn healthy (no status badge), catalog mid-build ("Building"),
 * pink-web unassigned ("Needs image").
 */
export const Populated: Story = {
  args: { seeded: projects },
};

/** No repositories registered → the EmptyState with the "Add repository" CTA. */
export const Empty: Story = {
  args: { seeded: [] },
};
