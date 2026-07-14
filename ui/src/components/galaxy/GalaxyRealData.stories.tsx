/**
 * Galaxy — real djinn data.
 *
 * Renders an actual `code_graph snapshot` of djinnos/djinn through the
 * production adapter (`snapshotToGalaxy`). The snapshot JSON lives in
 * `__fixtures__/djinn-code-graph.snapshot.json`, which is **gitignored**
 * (see `__fixtures__/.gitignore`) — it is a local preview artifact, not
 * repo content. Refresh it with:
 *
 *   code_graph { operation: "snapshot", project: "djinnos/djinn",
 *                level: "symbol", tests: "include", limit: 15000 }
 *
 * and drop the `.snapshot` object into that path. When the fixture is
 * absent the story renders a short how-to note instead of failing — so
 * this file is safe to commit while the data stays local.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";

import { GalaxyCanvas } from "./GalaxyCanvas";
import type { GalaxyData } from "./galaxyTypes";
import { snapshotToGalaxy } from "@/lib/codeGraphGalaxyAdapter";
import type { SnapshotPayload } from "@/lib/codeGraphAdapter";

// Optional import: eager glob resolves to {} when the fixture is deleted.
const fixtureModules = import.meta.glob<{ default: SnapshotPayload }>(
  "./__fixtures__/djinn-code-graph.snapshot.json",
  { eager: true },
);

const snapshot = Object.values(fixtureModules)[0]?.default ?? null;
const galaxy: GalaxyData | null = snapshot ? snapshotToGalaxy(snapshot) : null;

function RealDataHarness({
  colorMode,
  showLabels,
}: {
  colorMode: "stellar" | "heat";
  showLabels: boolean;
}) {
  if (!galaxy) {
    return (
      <div className="flex h-screen w-full items-center justify-center bg-[#04060c] font-mono text-sm text-slate-400">
        <div className="max-w-md space-y-2 text-center">
          <p className="text-slate-200">No local snapshot fixture.</p>
          <p>
            Fetch a `code_graph snapshot` of a project and save it as
            `src/components/galaxy/__fixtures__/djinn-code-graph.snapshot.json`
            (gitignored) to preview the galaxy on real data.
          </p>
        </div>
      </div>
    );
  }
  return (
    <div className="h-screen w-full">
      <GalaxyCanvas
        data={galaxy}
        colorMode={colorMode}
        showLabels={showLabels}
        title="djinnos/djinn — real snapshot"
      />
    </div>
  );
}

const meta = {
  title: "CodeGraph/GalaxyRealData",
  component: RealDataHarness,
  parameters: { layout: "fullscreen" },
  args: { colorMode: "stellar" as const, showLabels: false },
  argTypes: {
    colorMode: { control: "radio", options: ["stellar", "heat"] },
  },
} satisfies Meta<typeof RealDataHarness>;

export default meta;
type Story = StoryObj<typeof meta>;

/** The djinn codebase as a galaxy — degree-colored stellar scale. */
export const Djinn: Story = {};

/** Cognitive-complexity heat over the real graph. */
export const DjinnComplexityHeat: Story = {
  args: { colorMode: "heat" },
};
