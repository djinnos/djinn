/**
 * Environment/ProjectEnvironmentPage — the routed `/projects/:id/environment`
 * per-project config page.
 *
 * The page reads the project name from the real `projectStore` (seeded here)
 * and loads its config imperatively via `fetchEnvironmentConfig` →
 * `callMcpTool("project_environment_config_get")`. The header's
 * `ProjectImagePicker` separately lists the catalog through
 * `callMcpTool("image_list")` (react-query) and pre-selects `selected_image_id`.
 * A per-tool responder on the aliased `@/api/mcpClient` mock serves both.
 *
 * Image assignment is an admin-only, org-blast-radius setting, resolved through
 * `useAuthUser()` — which only works inside a real `AuthGate`. Like
 * `Navigation/Sidebar`, we install a one-time `window.fetch` shim (own guard
 * flag) answering `/auth/me`, `/setup/status` and `/auth/config`, and flip
 * `envStoryIsAdmin` per-story before `AuthGate` mounts. A per-story
 * `QueryClient` wraps it (the picker's catalog query needs one).
 */

import { useEffect } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { ProjectEnvironmentPage } from "./ProjectEnvironmentPage";
import { AuthGate } from "@/components/AuthGate";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";
import { projectStore } from "@/stores/projectStore";
import type { Project } from "@/api/types";

// ── Auth fetch shim (own guard flag; unknown URLs fall through) ───────────────

let envStoryIsAdmin = true;

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

if (!(window as unknown as { __djinnEnvFetchStub?: boolean }).__djinnEnvFetchStub) {
  (window as unknown as { __djinnEnvFetchStub?: boolean }).__djinnEnvFetchStub = true;
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
          is_admin: envStoryIsAdmin,
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
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  return (
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={[`/projects/${project.id}/environment`]}>
        <AuthGate>
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
        </AuthGate>
      </MemoryRouter>
    </QueryClientProvider>
  );
}

const meta = {
  title: "Repositories/ProjectEnvironmentPage",
  component: EnvironmentStory,
  parameters: { layout: "fullscreen" },
  decorators: [
    (Story, ctx) => {
      envStoryIsAdmin = (ctx.parameters?.isAdmin as boolean | undefined) ?? true;
      return <Story />;
    },
  ],
} satisfies Meta<typeof EnvironmentStory>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * Admin caller with two code-graph workspaces configured (a Rust server root, a
 * Node UI root), the "Rust + Node" catalog image pre-selected in the header
 * picker (interactive).
 */
export const Populated: Story = {
  parameters: { isAdmin: true },
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
  parameters: { isAdmin: true },
  beforeEach: () => setMcpToolResponder(makeResponder([])),
};

/**
 * Member caller: the header image picker renders read-only ("Rust + Node" as
 * plain text) — image assignment is admin-only. Workspaces stay editable.
 */
export const Member: Story = {
  parameters: { isAdmin: false },
  beforeEach: () =>
    setMcpToolResponder(
      makeResponder([
        { root: "server", language: "rust" },
        { root: "ui", language: "node" },
      ]),
    ),
};
