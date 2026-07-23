/**
 * Repositories/RepositoriesPage — the routed `/repositories` table.
 *
 * The page reads its rows from the real `projectStore` (seeded here) and each
 * row drives several MCP-backed sub-widgets:
 *   - `BranchPicker` fetches `['project', id, 'branches']` via
 *     `fetchProjectBranches` → `callMcpTool("project_branches")` (admins only).
 *   - the Image column reads `['project', id, 'environment-config']` via
 *     `fetchEnvironmentConfig` → `callMcpTool("project_environment_config_get")`;
 *     for admins the compact `ProjectImagePicker` also lists the catalog through
 *     `callMcpTool("image_list")`.
 *   - `ImageStatusBadge` polls `callMcpTool("get_project_devcontainer_status")`.
 * None of these go through a cache seam we can seed by key alone, so we install
 * a per-tool responder on the aliased `@/api/mcpClient` mock and branch on the
 * requested `project` id to give each row a distinct branch / image / status.
 *
 * Admin gating (target branch + image assignment are admin-only, org-blast-
 * radius settings) resolves through `useAuthUser()`, which only works inside a
 * real `AuthGate`. Exactly like `Navigation/Sidebar`, we install a one-time
 * `window.fetch` shim (under its OWN guard flag) that answers `/auth/me`,
 * `/setup/status` and `/auth/config`, and flip `repoStoryIsAdmin` per-story
 * BEFORE `AuthGate` mounts. A per-story `QueryClient` (retry:false) wraps it.
 */

import { useEffect } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { RepositoriesPage } from "./RepositoriesPage";
import { AuthGate } from "@/components/AuthGate";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";
import { projectStore } from "@/stores/projectStore";
import type { Project } from "@/api/types";

// ── Auth fetch shim (own guard flag; unknown URLs fall through) ───────────────

// Flipped from each story's decorator before AuthGate mounts and fires
// `/auth/me`, so the page gates the branch + image controls for the right caller.
let repoStoryIsAdmin = false;

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

if (!(window as unknown as { __djinnRepositoriesFetchStub?: boolean }).__djinnRepositoriesFetchStub) {
  (window as unknown as { __djinnRepositoriesFetchStub?: boolean }).__djinnRepositoriesFetchStub = true;
  const realFetch = window.fetch.bind(window);
  const stub: typeof window.fetch = (input, init) => {
    const url =
      typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
    if (url.includes("/auth/me")) {
      return Promise.resolve(
        json({
          id: "u-fernando",
          login: "fernando",
          name: "Fernando Bandeira",
          avatar_url: null,
          is_admin: repoStoryIsAdmin,
          role: "engineer",
        }),
      );
    }
    if (url.includes("/setup/status")) {
      return Promise.resolve(
        json({
          needs_app_install: false,
          app_credentials_configured: true,
          org_login: "djinnos",
          setup_state: "valid",
        }),
      );
    }
    if (url.includes("/auth/config")) {
      return Promise.resolve(
        json({
          configured: true,
          missing: [],
          setup_doc_url: "https://www.djinnai.io/docs/setup",
          self_setup_available: false,
        }),
      );
    }
    return realFetch(input, init);
  };
  window.fetch = stub;
}

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
    id: "project-worker",
    name: "djinnos/worker",
    github_owner: "djinnos",
    github_repo: "worker",
    branch: "main",
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
  "project-worker": ["main", "feat/cancel-path"],
  "project-web": ["main", "redesign", "wip/pricing"],
};

// Catalog for the admin image picker (config blobs are normalized by listImages).
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

// Per-project assigned catalog image (null → no image, "—" / None in the cell).
const imageByProject: Record<string, { id: string; name: string } | null> = {
  "project-djinn": { id: "img-rust-node", name: "Rust + Node" },
  "project-catalog": { id: "img-rust-node", name: "Rust + Node" },
  "project-worker": { id: "img-python", name: "Python 3.13" },
  "project-web": null,
};

// A distinct image/warm status per project so the row badges read differently:
// djinn ready (no badge), catalog mid-build, worker warming, pink-web needs an image.
function devcontainerStatusFor(projectId: string): unknown {
  switch (projectId) {
    case "project-catalog":
      return { image_status: "building", graph_warm_status: "pending", needs_image: false };
    case "project-worker":
      return { image_status: "ready", graph_warm_status: "warming", needs_image: false };
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
    case "project_environment_config_get": {
      const id = String(args?.project ?? "");
      const image = imageByProject[id] ?? null;
      return {
        status: "ok",
        config: { schema_version: 1, source: "user-edited", languages: {}, workspaces: [], system_packages: [], env: {}, lifecycle: {} },
        selected_image_id: image?.id ?? null,
        selected_image_name: image?.name ?? null,
      };
    }
    case "image_list":
      return { status: "ok", images: catalogImages };
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
        <AuthGate>
          <ProjectSeeder seeded={seeded}>
            <div className="h-screen bg-background text-foreground">
              <Routes>
                <Route path="/repositories" element={<RepositoriesPage />} />
                {/* Sinks for tab / row / action navigation. */}
                <Route path="/images" element={<div />} />
                <Route path="/tasks" element={<div />} />
                <Route path="/projects/:id/environment" element={<div />} />
              </Routes>
            </div>
          </ProjectSeeder>
        </AuthGate>
      </MemoryRouter>
    </QueryClientProvider>
  );
}

const meta = {
  title: "Repositories/RepositoriesPage",
  component: RepositoriesStory,
  parameters: { layout: "fullscreen" },
  args: { seeded: projects },
  beforeEach: () => {
    setMcpToolResponder(repositoriesResponder);
  },
  decorators: [
    (Story, ctx) => {
      repoStoryIsAdmin = (ctx.parameters?.isAdmin as boolean | undefined) ?? false;
      return <Story />;
    },
  ],
} satisfies Meta<typeof RepositoriesStory>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * Admin caller: the target-branch and image columns are interactive (branch
 * Select + compact `ProjectImagePicker`). Four repositories span the badge
 * states — djinn ready (no badge), catalog building, worker warming, pink-web
 * needs an image (and shows "—" in the Image column).
 */
export const Admin: Story = {
  parameters: { isAdmin: true },
};

/**
 * Member caller: target branch and image render as read-only plain text. Same
 * four repositories and image-status badges as Admin.
 */
export const Member: Story = {
  parameters: { isAdmin: false },
};

/** No repositories registered → the EmptyState with the "Add repository" CTA. */
export const Empty: Story = {
  args: { seeded: [] },
  parameters: { isAdmin: true },
};
