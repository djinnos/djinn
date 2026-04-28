import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";

import {
  ImpactFlowModal,
  type ImpactDetailedResult,
} from "@/components/codegraph/ImpactFlowModal";
import { Button } from "@/components/ui/button";

const target =
  "scip-rust . djinn-control-plane v1 src/state.rs#PoolManager#schedule()";

const baseEntries: ImpactDetailedResult["entries"] = [
  { key: "scip-rust . djinn-control-plane v1 src/bridge.rs#dispatch_task()", depth: 1 },
  { key: "scip-rust . djinn-control-plane v1 src/state.rs#enqueue_pulse()", depth: 1 },
  { key: "scip-rust . djinn-control-plane v1 src/tools/graph_tools.rs#warm()", depth: 1 },
  { key: "scip-rust . djinn-agent v1 src/actors/architect.rs#take_task()", depth: 2 },
  { key: "scip-rust . djinn-agent v1 src/actors/worker.rs#run_loop()", depth: 2 },
  { key: "scip-rust . djinn-server v1 src/handlers/http.rs#handle_pulse()", depth: 3 },
];

interface ModalDemoProps {
  impact: ImpactDetailedResult;
}

function ModalDemo({ impact }: ModalDemoProps) {
  const [open, setOpen] = useState(true);
  return (
    <div className="flex min-h-[60vh] items-center justify-center">
      <Button onClick={() => setOpen(true)}>Show impact</Button>
      <ImpactFlowModal
        open={open}
        onClose={() => setOpen(false)}
        impact={impact}
      />
    </div>
  );
}

const meta = {
  title: "Codegraph/ImpactFlowModal",
  component: ModalDemo,
  parameters: {
    layout: "fullscreen",
  },
} satisfies Meta<typeof ModalDemo>;

export default meta;

type Story = StoryObj<typeof meta>;

export const HighRisk: Story = {
  args: {
    impact: {
      key: target,
      target_label: "PoolManager::schedule",
      entries: baseEntries,
      risk: "HIGH",
      summary: "3 direct caller(s) across 3 module(s)",
    },
  },
};

export const CriticalRisk: Story = {
  args: {
    impact: {
      key: target,
      target_label: "PoolManager::schedule",
      entries: [
        ...baseEntries,
        ...Array.from({ length: 18 }, (_, i) => ({
          key: `scip-rust . crate v1 src/file_${i}.rs#caller_${i}()`,
          depth: 1,
        })),
      ],
      risk: "CRITICAL",
      summary: "21 direct caller(s) across 11 module(s)",
    },
  },
};

export const LowRisk: Story = {
  args: {
    impact: {
      key: target,
      target_label: "PoolManager::schedule",
      entries: [
        { key: "scip-rust . djinn-control-plane v1 src/bridge.rs#dispatch_task()", depth: 1 },
      ],
      risk: "LOW",
      summary: "1 direct caller(s) across 1 module(s)",
    },
  },
};

export const NoEntries: Story = {
  args: {
    impact: {
      key: target,
      target_label: "PoolManager::schedule",
      entries: [],
      risk: "LOW",
      summary: "no direct callers in current graph snapshot",
    },
  },
};
