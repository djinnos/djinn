/**
 * Memory/ScopeTreeNode — the recursive folder node used inside the Explorer's
 * "Scoped" section. Normally internal to `MemoryExplorer`, but it renders
 * standalone given a `ScopeTreeNode` value, so these stories build a real tree
 * from a scoped-notes fixture via `buildScopeTree` (exercising the same
 * single-child path compression the live UI relies on).
 */

import { useMemo, useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";

import { ScopeTreeNode } from "./ScopeTreeNode";
import { buildScopeTree } from "./memoryUtils";
import type { MemoryListOutputSchema } from "@/api/generated/mcp-tools.gen";

type NoteCompact = MemoryListOutputSchema.NoteCompact;

const daysAgo = (d: number): string =>
  new Date(Date.now() - d * 24 * 60 * 60 * 1000).toISOString();

const note = (
  id: string,
  title: string,
  note_type: string,
  scope_paths: string[],
  d: number,
): NoteCompact => ({
  id,
  title,
  note_type,
  permalink: `memory/${note_type}/${id}`,
  folder: `memory/${note_type}`,
  scope_paths: JSON.stringify(scope_paths),
  status: "active",
  updated_at: daysAgo(d),
});

// Deep, nested scopes so compression ("server/crates/…") and multi-level
// nesting are both visible.
const nestedNotes: NoteCompact[] = [
  note("oomkill", "Server OOMKill wiped tribunals", "pitfall", ["server/crates/djinn-server"], 0),
  note("actor-panic", "UTF-8 byte-slice panic kills the actor", "case", ["server/crates/djinn-server/actor"], 1),
  note("death-chain", "Tribunal death chain", "case", ["server/crates/djinn-coordinator"], 1),
  note("watchdog", "Refinement watchdog counts pending time", "pitfall", ["server/crates/djinn-memory"], 2),
  note("dedup-attr", "Write-dedup LLM unattributed", "pitfall", ["server/crates/djinn-memory/enrichment"], 2),
  note("sqlx-offline", "Dedicated DB + SQLX_OFFLINE per worktree", "pattern", ["server/crates/djinn-db"], 3),
  note("block-attr", "Proposal block attribute content dropped", "pitfall", ["ui/src/components/memory"], 4),
  note("sse-filter", "TaskStore holds all projects — don't filter SSE", "pitfall", ["ui/src/stores"], 5),
];

// A single flat folder — one level, several notes.
const singleFolderNotes: NoteCompact[] = [
  note("block-attr", "Proposal block attribute content dropped", "pitfall", ["ui/src/components/memory"], 1),
  note("body-format", "body_format markdown bypasses block validation", "pitfall", ["ui/src/components/memory"], 2),
  note("scope-tree", "Scope tree compresses single-child dirs", "pattern", ["ui/src/components/memory"], 3),
];

function ScopeTreeHarness({ notes }: { notes: NoteCompact[] }) {
  const tree = useMemo(() => buildScopeTree(notes), [notes]);
  const [selectedNoteId, setSelectedNoteId] = useState<string | null>(null);
  return (
    <aside className="h-screen w-80 space-y-1 overflow-y-auto border-r border-border bg-background p-2 text-foreground">
      {tree.map((node) => (
        <ScopeTreeNode
          key={node.fullPath}
          node={node}
          selectedNoteId={selectedNoteId}
          onSelectNote={(n) => setSelectedNoteId(n.id)}
          depth={1}
        />
      ))}
    </aside>
  );
}

const meta = {
  title: "Memory/ScopeTreeNode",
  component: ScopeTreeHarness,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof ScopeTreeHarness>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Multi-level tree with compressed single-child paths; click a note to select. */
export const NestedScopes: Story = {
  args: { notes: nestedNotes },
};

/** One folder holding a few notes. */
export const SingleFolder: Story = {
  args: { notes: singleFolderNotes },
};
