/**
 * Pages/Memory — the routed Memory page end to end.
 *
 * `MemoryPage` reads the selected project from `projectStore` and fetches its
 * knowledge base through `callMcpTool` (`memory_list` on mount, `memory_read`
 * on select, `memory_search` while typing). There is no TanStack Query seam
 * here, so we seed the real `projectStore` and install a per-tool responder on
 * the `@/api/mcpClient` mock that `.storybook/main.ts` aliases in at bundle
 * time. (We can't use `vi.mock` — it crashes the Vite preview runtime under
 * `storybook dev`; and Storybook 9.1's `sb.mock()` is a no-op on the Vite
 * builder.)
 *
 * The default view is the list (Explorer + detail); the graph view is covered
 * separately by `Memory/MemoryGraphCanvas` since its `view` state is internal
 * and can't be set from a story arg.
 */

import { useEffect } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { MemoryPage } from "./MemoryPage";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";
import { projectStore } from "@/stores/projectStore";
import type { Project } from "@/api/types";
import type { MemoryListOutputSchema } from "@/api/generated/mcp-tools.gen";

type NoteCompact = MemoryListOutputSchema.NoteCompact;

// ── Fixtures ─────────────────────────────────────────────────────────────────

const daysAgo = (d: number): string =>
  new Date(Date.now() - d * 24 * 60 * 60 * 1000).toISOString();
const hoursAgo = (h: number): string =>
  new Date(Date.now() - h * 60 * 60 * 1000).toISOString();

const note = (
  id: string,
  title: string,
  note_type: string,
  updated_at: string,
  scope_paths: string[] = [],
): NoteCompact => ({
  id,
  title,
  note_type,
  permalink: `memory/${note_type}/${id}`,
  folder: `memory/${note_type}`,
  scope_paths: JSON.stringify(scope_paths),
  status: "active",
  updated_at,
});

const listNotes: NoteCompact[] = [
  note("dolt-gone", "Postgres-only, Dolt retired", "adr", hoursAgo(6)),
  note("advisory-lock", "Coordinator advisory-lock leadership", "adr", daysAgo(2)),
  note("cutover", "Prefer cut-over over strangler migrations", "pattern", daysAgo(1)),
  note("openviking", "OpenViking — context DB for agents", "reference", daysAgo(5)),
  note("productivity", "Productivity 2x analysis 2026-07", "research", hoursAgo(20)),
  note("merge-queue", "Merge queue", "entity", daysAgo(4)),
  note("oomkill", "Server OOMKill wiped in-flight tribunals", "pitfall", hoursAgo(10), [
    "server/crates/djinn-server",
  ]),
  note("death-chain", "Tribunal death chain 2026-07-12", "case", hoursAgo(30), [
    "server/crates/djinn-coordinator",
  ]),
  note("watchdog", "Refinement watchdog counts pod pending time", "pitfall", daysAgo(1), [
    "server/crates/djinn-memory",
  ]),
  note("block-attr", "Proposal block attribute content dropped", "pitfall", daysAgo(3), [
    "ui/src/components/memory",
  ]),
];

const readNote = {
  id: "oomkill",
  title: "Server OOMKill wiped in-flight tribunals",
  note_type: "pitfall",
  permalink: "memory/pitfall/oomkill",
  status: "active",
  deduplicated: false,
  confidence: 0.86,
  scope_paths: ["server/crates/djinn-server"],
  tags: ["oom", "tribunals", "resolved"],
  updated_at: hoursAgo(10),
  content:
    "## Root cause\n\nA ~570MiB graph double-load plus a per-query deep clone tipped the pod over its 2Gi cgroup limit. See [[Tribunal death chain 2026-07-12]].\n\n## Fix\n\nBumped to **4Gi** and removed the per-query clone — shipped v0.6.94.",
};

// ── MCP responder (installed per story via `beforeEach`) ─────────────────────

function memoryResponder(tool: string) {
  switch (tool) {
    case "memory_list":
      return { notes: listNotes };
    case "memory_read":
      return readNote;
    case "memory_search":
      return { results: [] };
    case "memory_graph":
      return { nodes: [], edges: [] };
    case "memory_associations":
      return { associations: [] };
    default:
      return {};
  }
}

// ── Project store seeding ────────────────────────────────────────────────────

const project: Project = {
  id: "project-djinn",
  name: "djinnos/djinn",
  github_owner: "djinnos",
  github_repo: "djinn",
};

function ProjectSeeder({
  selectedProjectId,
  children,
}: {
  selectedProjectId: string | null;
  children: React.ReactNode;
}) {
  useEffect(() => {
    projectStore.setState({
      projects: [project],
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
  }, [selectedProjectId]);
  return <>{children}</>;
}

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, staleTime: Infinity } },
});

interface PageStoryArgs {
  selectedProjectId: string | null;
}

function MemoryPageStory({ selectedProjectId }: PageStoryArgs) {
  return (
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={["/memory"]}>
        <ProjectSeeder selectedProjectId={selectedProjectId}>
          <div className="h-screen bg-background text-foreground">
            <MemoryPage />
          </div>
        </ProjectSeeder>
      </MemoryRouter>
    </QueryClientProvider>
  );
}

const meta = {
  title: "Memory/MemoryPage",
  component: MemoryPageStory,
  parameters: { layout: "fullscreen" },
  beforeEach: () => {
    setMcpToolResponder(memoryResponder);
  },
} satisfies Meta<typeof MemoryPageStory>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Project selected: `memory_list` resolves and the Explorer tree populates. */
export const Populated: Story = {
  args: { selectedProjectId: project.id },
};

/** No project selected → the "Select a project to view its knowledge base" state. */
export const Empty: Story = {
  args: { selectedProjectId: null },
};
