/**
 * Memory/MemoryNoteDetail — the right pane that renders a single note's
 * frontmatter (type badge, scope paths, tags, confidence, updated time) and
 * its markdown body with wiki-style `[[links]]` rewired to `onNavigateToNote`.
 *
 * Pure props component — no fetching — so stories pass a `MemoryReadOutput`
 * fixture straight in. Note: this component renders frontmatter + body only;
 * it does NOT render a history timeline or the tasks/proposals back-refs that
 * `memory_read` also returns, so those fields are omitted from the fixtures.
 */

import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";

import { MemoryNoteDetail } from "./MemoryNoteDetail";
import type { MemoryReadOutput } from "@/api/generated/mcp-tools.gen";

const hoursAgo = (h: number): string =>
  new Date(Date.now() - h * 60 * 60 * 1000).toISOString();

// ── Fixtures ─────────────────────────────────────────────────────────────────

const richNote: MemoryReadOutput = {
  id: "note-oomkill",
  title: "Server OOMKill wiped all in-flight tribunals on restart",
  note_type: "pitfall",
  permalink: "memory/pitfall/server-oomkill",
  folder: "memory/pitfall",
  status: "active",
  deduplicated: false,
  confidence: 0.86,
  scope_paths: ["server/crates/djinn-server", "server/crates/djinn-coordinator"],
  tags: ["oom", "tribunals", "restart", "resolved"],
  created_at: hoursAgo(72),
  updated_at: hoursAgo(9),
  content: `## Symptom

The djinn server was OOMKilled at a **2Gi** limit and every in-flight
tribunal was stamped \`Interrupted\` on the subsequent restart. Users saw
work silently vanish mid-refinement.

## Root cause

Two compounding allocations under the same request:

1. A ~570MiB code graph was **double-loaded** into memory.
2. Every query took a **per-query deep clone** of that graph (see [[#1921]]).

At 2Gi headroom this tipped the pod over the cgroup limit under normal load.

## Fix

- Bumped the memory limit to **4Gi**.
- Landed the graph single-load + shared-reference changes in \`#1986\`,
  \`#1987\`, \`#1990\`, \`#1999\`, \`#2001\`.
- Verified restart-resume is live: one mid-flight tribunal resumed, zero
  \`Interrupted\` stamps.

Related: [[Tribunal death chain 2026-07-12]] and the
[[Refinement watchdog counts pod pending time]] pitfall share the same
restart-storm blast radius.

| Change | PR | Status |
| --- | --- | --- |
| 4Gi limit | #1990 | shipped v0.6.94 |
| graph single-load | #1986 | shipped v0.6.94 |
| deep-clone removal | #1999 | shipped v0.6.94 |

\`\`\`rust
// The offending path: a fresh clone per query.
let graph = self.graph.clone(); // ← 570MiB each time
\`\`\`
`,
};

const minimalNote: MemoryReadOutput = {
  id: "note-cutover",
  title: "Prefer cut-over over strangler migrations",
  note_type: "pattern",
  permalink: "memory/pattern/cutover-over-strangler",
  deduplicated: false,
  updated_at: hoursAgo(30),
  content:
    "When migrating a subsystem, do a **clean cut-over** rather than maintaining\na compatibility/strangler path. The dual-write window is where the subtle\nbugs live.",
};

// ── Harness ──────────────────────────────────────────────────────────────────

/**
 * Wraps the detail pane in a flex container (the component uses `flex-1`) and
 * surfaces the most-recent wikilink navigation target so the interaction is
 * observable in the story.
 */
function DetailHarness({
  note,
  loading,
}: {
  note: MemoryReadOutput | null;
  loading: boolean;
}) {
  const [navigatedTo, setNavigatedTo] = useState<string | null>(null);
  return (
    <div className="flex h-screen flex-col bg-background text-foreground">
      {navigatedTo && (
        <div className="shrink-0 border-b border-border bg-white/[0.03] px-4 py-1.5 text-xs text-muted-foreground">
          wikilink clicked → navigate to: <span className="text-blue-400">{navigatedTo}</span>
        </div>
      )}
      <div className="flex min-h-0 flex-1">
        <MemoryNoteDetail
          note={note}
          loading={loading}
          onNavigateToNote={setNavigatedTo}
        />
      </div>
    </div>
  );
}

// ── Meta / stories ───────────────────────────────────────────────────────────

const meta = {
  title: "Memory/MemoryNoteDetail",
  component: DetailHarness,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof DetailHarness>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Full note: type badge, scope paths, tags, confidence meter, wikilinks,
 * table and a fenced code block. Click a `[[link]]` to fire onNavigateToNote. */
export const RichNote: Story = {
  args: { note: richNote, loading: false },
};

/** A sparse note with only a body — no scope/tags/confidence chrome. */
export const MinimalNote: Story = {
  args: { note: minimalNote, loading: false },
};

/** Detail-fetch spinner. */
export const Loading: Story = {
  args: { note: null, loading: true },
};

/** Nothing selected yet — the "Select a note" placeholder. */
export const Empty: Story = {
  args: { note: null, loading: false },
};
