/**
 * Memory/MemoryExplorer — the left rail of the Memory page.
 *
 * `MemoryExplorer` is a pure props component: it takes the compact notes
 * list, the current search query + results, and the selected-note id, and
 * emits `onSearchChange` / `onSelectNote`. No fetching happens inside, so the
 * stories drive it entirely through a small stateful harness — no query
 * seeding or MCP mocking required.
 */

import { useMemo, useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";

import { MemoryExplorer } from "./MemoryExplorer";
import type {
  MemoryListOutputSchema,
  MemorySearchOutputSchema,
} from "@/api/generated/mcp-tools.gen";

type NoteCompact = MemoryListOutputSchema.NoteCompact;
type SearchResult = MemorySearchOutputSchema.MemorySearchResultItem;

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

/**
 * A plausible knowledge base for an AI-agent platform: global architecture
 * decisions / patterns / references plus scoped pitfalls and cases that carry
 * `scope_paths` and thus fall into the Scoped tree.
 */
const explorerNotes: NoteCompact[] = [
  // Global — decisions / patterns / references / research / enrichment
  note("dolt-gone", "Postgres-only, Dolt retired", "adr", hoursAgo(6)),
  note("advisory-lock", "Coordinator advisory-lock leadership", "adr", daysAgo(2)),
  note("cutover-strangler", "Prefer cut-over over strangler migrations", "pattern", daysAgo(1)),
  note("per-user-pool", "Per-user elastic concurrency pool", "pattern", daysAgo(3)),
  note("openviking", "OpenViking — context DB for agents", "reference", daysAgo(5)),
  note("okf-spec", "OKF Open Knowledge Format spec", "reference", daysAgo(9)),
  note("productivity-2x", "Productivity 2x analysis 2026-07", "research", hoursAgo(20)),
  note("merge-queue-entity", "Merge queue", "entity", daysAgo(4)),
  note("taskpod-cpu-claim", "Task pods run 4 vCPU in prod", "claim", daysAgo(2)),

  // Scoped — pitfalls / cases / patterns with scope_paths (Scoped tree)
  note(
    "oomkill",
    "Server OOMKill wiped in-flight tribunals",
    "pitfall",
    hoursAgo(10),
    ["server/crates/djinn-server"],
  ),
  note(
    "death-chain",
    "Tribunal death chain 2026-07-12",
    "case",
    hoursAgo(30),
    ["server/crates/djinn-coordinator"],
  ),
  note(
    "watchdog-pending",
    "Refinement watchdog counts pod pending time",
    "pitfall",
    daysAgo(1),
    ["server/crates/djinn-memory"],
  ),
  note(
    "cargo-flock",
    "Cargo target seeder hardlinks Cargo.lock",
    "pitfall",
    daysAgo(2),
    ["server/crates/djinn-worker/cache"],
  ),
  note(
    "block-attr-dropped",
    "Proposal block attribute content dropped",
    "pitfall",
    daysAgo(3),
    ["ui/src/components/memory"],
  ),
  note(
    "sqlx-offline",
    "Dedicated DB + SQLX_OFFLINE per worktree",
    "pattern",
    daysAgo(4),
    ["server/crates/djinn-db"],
  ),
];

// ── Harness ──────────────────────────────────────────────────────────────────

interface ExplorerHarnessProps {
  notes: NoteCompact[];
  initialQuery?: string;
  initialSelectedId?: string | null;
}

/**
 * Wires the controlled props MemoryExplorer expects. Search results are
 * computed synchronously from the fixture (a title substring match) so the
 * "search" path renders real result rows without a server.
 */
function ExplorerHarness({
  notes,
  initialQuery = "",
  initialSelectedId = null,
}: ExplorerHarnessProps) {
  const [query, setQuery] = useState(initialQuery);
  const [selectedNoteId, setSelectedNoteId] = useState<string | null>(
    initialSelectedId,
  );

  const searchResults = useMemo<SearchResult[] | null>(() => {
    const q = query.trim().toLowerCase();
    if (!q) return null;
    return notes
      .filter((n) => n.title.toLowerCase().includes(q))
      .map<SearchResult>((n) => ({
        id: n.id,
        title: n.title,
        note_type: n.note_type,
        permalink: n.permalink,
        folder: n.folder,
        snippet: `…the note "${n.title}" mentions ${query}…`,
        score: 0.9,
        entity: "note",
      }));
  }, [query, notes]);

  return (
    <div className="flex h-screen bg-background text-foreground">
      <MemoryExplorer
        notes={notes}
        searchQuery={query}
        onSearchChange={setQuery}
        searchResults={searchResults}
        selectedNoteId={selectedNoteId}
        onSelectNote={(n) => setSelectedNoteId(n.id)}
      />
      <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
        Detail pane — selected note id: {selectedNoteId ?? "(none)"}
      </div>
    </div>
  );
}

// ── Meta / stories ───────────────────────────────────────────────────────────

const meta = {
  title: "Memory/MemoryExplorer",
  component: ExplorerHarness,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof ExplorerHarness>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Populated Global + Scoped tree. Expand the sections/folders to browse. */
export const Populated: Story = {
  args: { notes: explorerNotes },
};

/** A note is pre-selected so the highlighted row styling is visible. */
export const WithSelection: Story = {
  args: { notes: explorerNotes, initialSelectedId: "oomkill" },
};

/** Search view: the query is seeded and matching result rows are rendered. */
export const SearchResults: Story = {
  args: { notes: explorerNotes, initialQuery: "refinement" },
};

/** Search with a query that matches nothing → "No results found". */
export const SearchNoResults: Story = {
  args: { notes: explorerNotes, initialQuery: "zzz-nonexistent" },
};

/** Empty knowledge base — both sections show their empty placeholders. */
export const Empty: Story = {
  args: { notes: [] },
};
