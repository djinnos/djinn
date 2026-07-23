/**
 * Navigation/Sidebar — the app's left navigation rail.
 *
 * The Sidebar pulls from four independent seams, each mocked here:
 *   - `useAuthUser()` (the footer identity + the admin-only Usage/Users items)
 *     resolves only inside a real `AuthGate`, which performs REST calls to
 *     `/auth/me`, `/setup/status` and `/auth/config`. Exactly like
 *     `Settings/SettingsPage`, we install a one-time `window.fetch` shim (under
 *     its OWN guard flag so it never fights the Settings shim) that answers just
 *     those endpoints and falls through for everything else. `sidebarIsAdmin` is
 *     flipped per-story BEFORE `AuthGate` mounts so `/auth/me` reports the right
 *     flag.
 *   - The Proposals badge counts non-terminal proposals from
 *     `proposalListQueryOptions()` → `callMcpTool("proposal_list")`.
 *   - The Repositories warning badge counts projects whose image build
 *     `failed`, via `useDevcontainerWarnings` →
 *     `callMcpTool("get_project_devcontainer_status")` fanned out over the
 *     seeded `projectStore` projects.
 * Both MCP-backed seams are served by a per-story responder installed in
 * `beforeEach`.
 */

import { useEffect } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { Sidebar } from "./Sidebar";
import { AuthGate } from "@/components/AuthGate";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";
import { projectStore } from "@/stores/projectStore";
import type { Project } from "@/api/types";
import type { ProposalListRow } from "@/api/types";

// ── Auth fetch shim (own guard flag; unknown URLs fall through) ───────────────

// Flipped from each story's decorator before AuthGate mounts and fires
// `/auth/me`, so the footer + admin items render for the right caller.
let sidebarIsAdmin = false;

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

if (!(window as unknown as { __djinnSidebarFetchStub?: boolean }).__djinnSidebarFetchStub) {
  (window as unknown as { __djinnSidebarFetchStub?: boolean }).__djinnSidebarFetchStub = true;
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
          is_admin: sidebarIsAdmin,
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
  { id: "project-djinn", name: "djinnos/djinn", github_owner: "djinnos", github_repo: "djinn" },
  { id: "project-catalog", name: "djinnos/catalog", github_owner: "djinnos", github_repo: "catalog" },
  { id: "project-web", name: "djinnos/pink-web", github_owner: "djinnos", github_repo: "pink-web" },
];

const ISO = (msAgo: number): string => new Date(Date.now() - msAgo).toISOString();

function row(id: string, status: string, title: string): ProposalListRow {
  return {
    id,
    short_id: id,
    title,
    status,
    ac_met: 2,
    ac_total: 3,
    pending_reconcile: false,
    created_at: ISO(86_400_000),
    updated_at: ISO(3_600_000),
  };
}

// Four non-terminal proposals → the Proposals nav badge reads "4".
const activeProposals: ProposalListRow[] = [
  row("prop-warm", "building", "Graph-warm lease fault matrix"),
  row("prop-sweep", "in_review", "Proposal integrity sweep canary"),
  row("prop-park", "approved", "Atomic source-intent park transitions"),
  row("prop-broker", "draft", "Broker command rejection semantics"),
];

/**
 * Two of the three seeded projects report a `failed` image build → the
 * Repositories nav item shows an amber "2" warning badge. "building" is
 * excluded from the count by design (it's informational).
 */
function devcontainerStatusFor(projectId: string): unknown {
  if (projectId === "project-djinn" || projectId === "project-web") {
    return { image_status: "failed", graph_warm_status: "failed", needs_image: false };
  }
  return { image_status: "ready", graph_warm_status: "ready", needs_image: false };
}

function busyResponder(name: string, args: Record<string, unknown> | undefined): unknown {
  switch (name) {
    case "proposal_list":
      return { proposals: activeProposals };
    case "get_project_devcontainer_status":
      return devcontainerStatusFor(String(args?.project ?? ""));
    default:
      return {};
  }
}

function quietResponder(name: string): unknown {
  switch (name) {
    case "proposal_list":
      return { proposals: [] };
    case "get_project_devcontainer_status":
      return { image_status: "ready", graph_warm_status: "ready", needs_image: false };
    default:
      return {};
  }
}

// ── Harness ──────────────────────────────────────────────────────────────────

function ProjectSeeder({ children }: { children: React.ReactNode }) {
  useEffect(() => {
    projectStore.setState({
      projects,
      selectedProjectId: projects[0]!.id,
      lastViewPerProject: {},
    });
    return () => {
      projectStore.setState({ projects: [], selectedProjectId: null, lastViewPerProject: {} });
    };
  }, []);
  return <>{children}</>;
}

interface SidebarStoryArgs {
  initialPath: string;
}

function SidebarStory({ initialPath }: SidebarStoryArgs) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  return (
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={[initialPath]}>
        <AuthGate>
          <ProjectSeeder>
            <div className="flex h-screen bg-background text-foreground">
              <Sidebar />
              <div className="flex-1" />
            </div>
          </ProjectSeeder>
        </AuthGate>
      </MemoryRouter>
    </QueryClientProvider>
  );
}

const meta = {
  title: "Navigation/Sidebar",
  component: SidebarStory,
  parameters: { layout: "fullscreen" },
  args: { initialPath: "/tasks" },
  decorators: [
    (Story, ctx) => {
      sidebarIsAdmin = (ctx.parameters?.isAdmin as boolean | undefined) ?? false;
      return <Story />;
    },
  ],
} satisfies Meta<typeof SidebarStory>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * Member caller on the Tasks view: the Proposals badge shows "4" pending, the
 * Repositories item shows an amber "2" warning, and the footer shows the signed
 * in user. Admin-only Usage/Users items are hidden.
 */
export const Default: Story = {
  parameters: { isAdmin: false },
  beforeEach: () => setMcpToolResponder(busyResponder),
};

/**
 * Admin caller: the footer reveals the admin-only Usage and Users items above
 * Settings. Same proposal + devcontainer badges as Default.
 */
export const Admin: Story = {
  parameters: { isAdmin: true },
  beforeEach: () => setMcpToolResponder(busyResponder),
};

/**
 * Nothing pending: no proposals in flight and every project's image is healthy,
 * so both badges are absent — the resting state of the rail.
 */
export const Quiet: Story = {
  parameters: { isAdmin: false },
  beforeEach: () => setMcpToolResponder(quietResponder),
};

/** Repositories active (the URL drives the highlighted section). */
export const RepositoriesActive: Story = {
  args: { initialPath: "/repositories" },
  parameters: { isAdmin: true },
  beforeEach: () => setMcpToolResponder(busyResponder),
};

/**
 * `/images` is part of the Repositories section ("hub") — there is no separate
 * Images nav item, and the Repositories item stays highlighted on that route.
 */
export const ImagesActive: Story = {
  args: { initialPath: "/images" },
  parameters: { isAdmin: true },
  beforeEach: () => setMcpToolResponder(busyResponder),
};
