/**
 * Proposals/ProposalsPage — the routed Proposals experience end to end.
 *
 * `ProposalsPage` branches on the `:id` route param: no id renders the grouped
 * list (`proposal_list` via `callMcpTool`), an id renders the detail
 * (`proposal_show`). Neither goes through a TanStack Query seam we can seed by
 * key alone, so — exactly like `Pages/Memory` — we install a per-tool responder
 * on the `@/api/mcpClient` mock that `.storybook/main.ts` aliases in at bundle
 * time. The org-users list is fetched over REST (not MCP), so that one we seed
 * straight into the QueryClient cache.
 *
 * `useAuthUser()` returns null in Storybook (the auth context is only provided
 * by the real `AuthGate`, which performs network calls), so permission-gated
 * affordances — sign-off buttons, the kick-off button, "Ask djinn" — do not
 * render here. Every data-driven panel (spec, tribunal review, readiness gate,
 * sign-off states, revision history, debate trail, feedback thread) does.
 */

import { useEffect } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { ProposalsPage } from "./ProposalsPage";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";
import { projectStore } from "@/stores/projectStore";
import type { Project } from "@/api/types";
import {
  listProposals,
  richDetail,
  users,
} from "@/components/proposals/proposalStoryFixtures";

// ── Fixtures ─────────────────────────────────────────────────────────────────

const projects: Project[] = [
  {
    id: "project-djinn",
    name: "djinnos/djinn",
    github_owner: "djinnos",
    github_repo: "djinn",
  },
  {
    id: "project-catalog",
    name: "djinnos/catalog",
    github_owner: "djinnos",
    github_repo: "catalog",
  },
];

// ── MCP responders (installed per story via `beforeEach`) ─────────────────────

function populatedResponder(tool: string): unknown {
  switch (tool) {
    case "proposal_list":
      return { proposals: listProposals };
    case "proposal_show":
      return richDetail;
    default:
      return {};
  }
}

function emptyResponder(tool: string): unknown {
  switch (tool) {
    case "proposal_list":
      return { proposals: [] };
    default:
      return {};
  }
}

// ── Store + query seeding ────────────────────────────────────────────────────

function makeQueryClient(): QueryClient {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  // Org users are fetched over REST, not MCP — seed the cache directly.
  qc.setQueryData(["users", "list"], users);
  return qc;
}

function StoreSeeder({ children }: { children: React.ReactNode }) {
  useEffect(() => {
    projectStore.setState({
      projects,
      selectedProjectId: projects[0]!.id,
      lastViewPerProject: {},
    });
    return () => {
      projectStore.setState({
        projects: [],
        selectedProjectId: null,
        lastViewPerProject: {},
      });
    };
  }, []);
  return <>{children}</>;
}

function ProposalsHarness({ initialPath }: { initialPath: string }) {
  return (
    <QueryClientProvider client={makeQueryClient()}>
      <MemoryRouter initialEntries={[initialPath]}>
        <StoreSeeder>
          <div className="h-screen bg-background text-foreground">
            <Routes>
              <Route path="/proposals" element={<ProposalsPage />} />
              <Route path="/proposals/:id" element={<ProposalsPage />} />
              {/* Sinks for in-page navigation (row click, chat, epic board). */}
              <Route path="/chat" element={<div />} />
              <Route path="/tasks" element={<div />} />
            </Routes>
          </div>
        </StoreSeeder>
      </MemoryRouter>
    </QueryClientProvider>
  );
}

const meta = {
  title: "Proposals/ProposalsPage",
  component: ProposalsHarness,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof ProposalsHarness>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * The grouped list with rows across the lifecycle (building, approved,
 * in-review/awaiting-review, refining, blocked, needs-evidence, triage,
 * rejected). Each non-terminal row shows its tribunal chip + gate dot.
 */
export const Populated: Story = {
  args: { initialPath: "/proposals" },
  beforeEach: () => {
    setMcpToolResponder(populatedResponder);
  },
};

/** No proposals returned → the empty "No proposals yet." state. */
export const Empty: Story = {
  args: { initialPath: "/proposals" },
  beforeEach: () => {
    setMcpToolResponder(emptyResponder);
  },
};

/**
 * The detail open on the rich proposal (route param drives selection). Shows
 * the spec, the converged tribunal review card (judge verdict / spec diff /
 * debate trail behind tabs), the blocked readiness gate (DoR failures + judge
 * needs-work + a blocking entry), sign-offs, revision history, and the human
 * feedback thread.
 */
export const DetailOpen: Story = {
  args: { initialPath: "/proposals/prop-refine" },
  beforeEach: () => {
    setMcpToolResponder(populatedResponder);
  },
};
