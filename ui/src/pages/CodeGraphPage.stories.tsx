import { useEffect } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { CodeGraphPage } from "@/pages/CodeGraphPage";
import { projectStore } from "@/stores/projectStore";
import type { Project } from "@/api/types";

/**
 * PR D1 stories. The page is intentionally inert here — D2 will add a story
 * that seeds a fixture graph through `useSigmaGraph`, but for the empty
 * scaffolding we just want to verify the layout renders in both states.
 */

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, staleTime: Infinity } },
});

const fixtureProjects: Project[] = [
  {
    id: "project-djinn",
    name: "djinnos/djinn",
    github_owner: "djinnos",
    github_repo: "djinn",
    created_at: "2026-01-01T00:00:00.000Z",
    updated_at: "2026-04-01T00:00:00.000Z",
  },
  {
    id: "project-sigma-demo",
    name: "djinnos/sigma-demo",
    github_owner: "djinnos",
    github_repo: "sigma-demo",
    created_at: "2026-02-01T00:00:00.000Z",
    updated_at: "2026-03-15T00:00:00.000Z",
  },
];

interface ProjectStoreSeederProps {
  projects: Project[];
  selectedProjectId: string | null;
  children: React.ReactNode;
}

function ProjectStoreSeeder({
  projects,
  selectedProjectId,
  children,
}: ProjectStoreSeederProps) {
  useEffect(() => {
    projectStore.setState({
      projects,
      selectedProjectId,
      lastViewPerProject: {},
    });
    return () => {
      projectStore.setState({
        projects: [],
        selectedProjectId: null,
        lastViewPerProject: {},
      });
    };
  }, [projects, selectedProjectId]);

  return <>{children}</>;
}

const meta = {
  title: "Pages/CodeGraphPage",
  component: CodeGraphPage,
  parameters: {
    layout: "fullscreen",
  },
  decorators: [
    (Story, ctx) => {
      const seed = (ctx.parameters?.seed as ProjectStoreSeederProps | undefined) ?? {
        projects: fixtureProjects,
        selectedProjectId: fixtureProjects[0]!.id,
        children: null,
      };
      return (
        <QueryClientProvider client={queryClient}>
          <MemoryRouter initialEntries={["/code-graph"]}>
            <ProjectStoreSeeder
              projects={seed.projects}
              selectedProjectId={seed.selectedProjectId}
            >
              <div className="h-screen">
                <Story />
              </div>
            </ProjectStoreSeeder>
          </MemoryRouter>
        </QueryClientProvider>
      );
    },
  ],
} satisfies Meta<typeof CodeGraphPage>;

export default meta;

type Story = StoryObj<typeof meta>;

export const EmptyCanvas: Story = {
  parameters: {
    seed: {
      projects: fixtureProjects,
      selectedProjectId: fixtureProjects[0]!.id,
    },
  },
};

export const NoProjectSelected: Story = {
  parameters: {
    seed: {
      projects: fixtureProjects,
      selectedProjectId: null,
    },
  },
};

export const NoProjectsConfigured: Story = {
  parameters: {
    seed: {
      projects: [],
      selectedProjectId: null,
    },
  },
};
