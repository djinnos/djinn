import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";

import { QueryPalette } from "./QueryPalette";
import type { SearchHit } from "@/api/codeGraph";

const sampleHits: SearchHit[] = [
  {
    key: "scip:rust . djinn . :: actors :: slot :: helpers :: warm_canonical_graph()",
    kind: "function",
    display_name: "warm_canonical_graph",
    score: 0.91,
    file: "server/crates/djinn-agent/src/actors/slot/helpers.rs",
    match_kind: "hybrid",
  },
  {
    key: "scip:rust . djinn . :: graph :: build_canonical()",
    kind: "function",
    display_name: "build_canonical",
    score: 0.78,
    file: "server/crates/djinn-graph/src/repo_graph.rs",
    match_kind: "lexical",
  },
  {
    key: "scip:rust . djinn . :: agent :: ctx :: AgentCtx#",
    kind: "struct",
    display_name: "AgentCtx",
    score: 0.61,
    file: "server/crates/djinn-agent/src/ctx.rs",
    match_kind: "semantic",
  },
];

function StoryShell({
  hits,
  startOpen = true,
}: {
  hits?: SearchHit[];
  startOpen?: boolean;
}) {
  const [open, setOpen] = useState(startOpen);
  return (
    <div className="flex h-[400px] w-[600px] flex-col items-center justify-center bg-background">
      <button
        type="button"
        onClick={() => setOpen(true)}
        className="rounded-md border border-border/60 bg-background px-3 py-1.5 text-sm"
      >
        Open palette (⌘K)
      </button>
      <QueryPalette
        projectId="storybook"
        open={open}
        onOpenChange={setOpen}
        injectedHits={hits}
      />
    </div>
  );
}

const meta: Meta<typeof StoryShell> = {
  title: "CodeGraph/QueryPalette",
  component: StoryShell,
  parameters: { layout: "centered" },
};
export default meta;
type Story = StoryObj<typeof StoryShell>;

export const Empty: Story = { args: { hits: [], startOpen: true } };

export const WithHits: Story = {
  args: { hits: sampleHits, startOpen: true },
};

export const Closed: Story = { args: { hits: [], startOpen: false } };
