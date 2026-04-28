import { useEffect } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";

import { SymbolDetailPanel } from "./SymbolDetailPanel";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import type { SymbolContext } from "@/api/codeGraph";

const sampleContext: SymbolContext = {
  symbol: {
    uid: "scip:rust . djinn . :: actors :: slot :: helpers :: warm_canonical_graph()",
    name: "warm_canonical_graph",
    kind: "function",
    file_path: "server/crates/djinn-agent/src/actors/slot/helpers.rs",
    start_line: 828,
    end_line: 1017,
    content: null,
    method_metadata: {
      visibility: "pub(crate)",
      is_async: true,
      params: [
        { name: "ctx", type_name: "&AgentCtx", default_value: null },
        {
          name: "force_refresh",
          type_name: "bool",
          default_value: "false",
        },
      ],
      return_type: "Result<CanonicalGraph, AgentError>",
      annotations: ["#[tracing::instrument(skip(ctx))]"],
    },
  },
  incoming: {
    calls: [
      {
        uid: "scip:rust . djinn . :: actors :: architect :: patrol()",
        name: "patrol",
        kind: "function",
        file_path: "server/crates/djinn-agent/src/actors/architect.rs",
        confidence: 0.95,
      },
      {
        uid: "scip:rust . djinn . :: actors :: slot :: helpers :: ensure_warm()",
        name: "ensure_warm",
        kind: "function",
        file_path: "server/crates/djinn-agent/src/actors/slot/helpers.rs",
        confidence: 0.9,
      },
    ],
  },
  outgoing: {
    calls: [
      {
        uid: "scip:rust . djinn . :: graph :: build_canonical()",
        name: "build_canonical",
        kind: "function",
        file_path: "server/crates/djinn-graph/src/repo_graph.rs",
        confidence: 0.92,
      },
    ],
    reads: [
      {
        uid: "scip:rust . djinn . :: agent :: ctx :: AgentCtx",
        name: "AgentCtx",
        kind: "struct",
        file_path: "server/crates/djinn-agent/src/ctx.rs",
        confidence: 0.85,
      },
    ],
  },
  processes: [],
};

function StoryShell({ context }: { context: SymbolContext | null }) {
  const setSelection = useCodeGraphStore((s) => s.setSelection);
  useEffect(() => {
    setSelection(context ? context.symbol.uid : null);
    return () => setSelection(null);
  }, [context, setSelection]);
  return (
    <div className="flex h-[600px] w-[400px] border border-border/40 bg-background">
      <SymbolDetailPanel projectId="storybook" injectedContext={context} />
    </div>
  );
}

const meta: Meta<typeof StoryShell> = {
  title: "CodeGraph/SymbolDetailPanel",
  component: StoryShell,
  parameters: { layout: "centered" },
};
export default meta;
type Story = StoryObj<typeof StoryShell>;

export const Closed: Story = {
  args: { context: null },
};

export const FullyPopulated: Story = {
  args: { context: sampleContext },
};

export const NoMethodMetadata: Story = {
  args: {
    context: {
      ...sampleContext,
      symbol: { ...sampleContext.symbol, method_metadata: null },
    },
  },
};

export const NoEdges: Story = {
  args: {
    context: {
      ...sampleContext,
      incoming: {},
      outgoing: {},
    },
  },
};
