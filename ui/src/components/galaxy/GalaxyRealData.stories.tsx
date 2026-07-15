/**
 * Galaxy — real djinn data, full graph.
 *
 * Renders an actual local `dump_full_galaxy_snapshot` dump (every node,
 * every edge — no server caps) through the production adapter. The
 * snapshot JSON lives in `__fixtures__/djinn-code-graph.snapshot.json`,
 * which is **gitignored** (see `__fixtures__/.gitignore`) — a local
 * preview artifact, not repo content. Refresh it with the ignored dev
 * test documented in `server/crates/djinn-graph/src/dev_fixture_dump.rs`.
 *
 * The layout runs in the shared Web Worker (a whole-repo graph takes a
 * while — the story shows progress instead of freezing the tab). When
 * the fixture is absent the story renders a how-to note instead of
 * failing, so this file is safe to commit while the data stays local.
 */

import { useEffect, useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";

import { GalaxyCanvas } from "./GalaxyCanvas";
import { layoutInWorker } from "./galaxyLayoutClient";
import type { GalaxyData } from "./galaxyTypes";
import {
  galaxyLayoutSeed,
  snapshotToGalaxy,
} from "@/lib/codeGraphGalaxyAdapter";
import type { SnapshotPayload } from "@/lib/codeGraphAdapter";
import { CodeGraphPage } from "@/pages/CodeGraphPage";
import { projectStore } from "@/stores/projectStore";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";

// Optional import: eager glob resolves to {} when the fixture is deleted.
const fixtureModules = import.meta.glob<{ default: SnapshotPayload }>(
  "./__fixtures__/djinn-code-graph.snapshot.json",
  { eager: true },
);

const snapshot = Object.values(fixtureModules)[0]?.default ?? null;

// Adapt once (cheap), lay out once per session (expensive, in the worker),
// then cache for every story/args change.
let galaxyPromise: Promise<GalaxyData> | null = null;
function loadGalaxy(): Promise<GalaxyData> {
  if (!snapshot) return Promise.reject(new Error("no fixture"));
  galaxyPromise ??= (async () => {
    const data = snapshotToGalaxy(snapshot, { layout: false });
    const nodes = await layoutInWorker(
      data.nodes,
      data.edges,
      galaxyLayoutSeed(snapshot.project_id),
    );
    return { ...data, nodes };
  })();
  return galaxyPromise;
}

function RealDataHarness({
  colorMode,
  showLabels,
}: {
  colorMode: "group" | "heat";
  showLabels: boolean;
}) {
  const [galaxy, setGalaxy] = useState<GalaxyData | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    if (!snapshot) return;
    let cancelled = false;
    loadGalaxy()
      .then((data) => {
        if (!cancelled) setGalaxy(data);
      })
      .catch(() => {
        if (!cancelled) setFailed(true);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  if (!snapshot || failed) {
    return (
      <div className="flex h-screen w-full items-center justify-center bg-[#06090f] font-mono text-sm text-slate-400">
        <div className="max-w-md space-y-2 text-center">
          <p className="text-slate-200">No local snapshot fixture.</p>
          <p>
            Run the `dump_full_galaxy_snapshot` dev test (see
            `server/crates/djinn-graph/src/dev_fixture_dump.rs`) to write
            `src/components/galaxy/__fixtures__/djinn-code-graph.snapshot.json`
            (gitignored) and preview the galaxy on the full real graph.
          </p>
        </div>
      </div>
    );
  }

  if (!galaxy) {
    return (
      <div className="flex h-screen w-full items-center justify-center bg-[#06090f] font-mono text-sm text-slate-400">
        <div className="space-y-2 text-center">
          <span
            className="mx-auto block h-5 w-5 animate-spin rounded-full border-2 border-slate-600 border-t-slate-200"
            role="status"
            aria-label="Computing layout"
          />
          <p>
            Computing layout for {snapshot.nodes.length.toLocaleString()} nodes…
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
        title="djinnos/djinn — full local dump"
      />
    </div>
  );
}

const meta = {
  title: "CodeGraph/GalaxyRealData",
  component: RealDataHarness,
  parameters: { layout: "fullscreen" },
  args: { colorMode: "group" as const, showLabels: false },
  argTypes: {
    colorMode: { control: "radio", options: ["group", "heat"] },
  },
} satisfies Meta<typeof RealDataHarness>;

export default meta;
type Story = StoryObj<typeof meta>;

/** The djinn codebase as a galaxy — per-crate colors, whole graph. */
export const Djinn: Story = {};

/** Cognitive-complexity heat over the real graph. */
export const DjinnComplexityHeat: Story = {
  args: { colorMode: "heat" },
};

// ── Full page: the real /code-graph experience ──────────────────────────────
//
// Mounts the actual `CodeGraphPage` (GalaxyView + Cmd-K palette + FAB)
// against the full local dump via the aliased mcpClient mock, with the
// project store seeded — project chip, workspace picker, hide-tests,
// color modes, and search all live exactly as on the real page.

function FullPageHarness() {
  useEffect(() => {
    projectStore.setState({
      projects: [
        {
          id: "project-djinn",
          name: "djinn",
          github_owner: "djinnos",
          github_repo: "djinn",
        },
      ],
      selectedProjectId: "project-djinn",
      lastViewPerProject: {},
    });
  }, []);

  if (!snapshot) {
    return (
      <div className="flex h-screen w-full items-center justify-center bg-[#06090f] font-mono text-sm text-slate-400">
        No local snapshot fixture — see the note on the Djinn story.
      </div>
    );
  }
  return (
    <div className="h-screen w-full">
      <CodeGraphPage />
    </div>
  );
}

export const FullPage: StoryObj = {
  render: () => <FullPageHarness />,
  beforeEach: () => {
    setMcpToolResponder((name, args) => {
      if (name !== "code_graph" || !snapshot) return {};
      if (args?.operation === "snapshot") return { snapshot };
      if (args?.operation === "workspaces") {
        const slugs = new Set<string>();
        for (const node of snapshot.nodes) {
          if (node.workspace) slugs.add(node.workspace);
        }
        return {
          workspaces: [...slugs].map((slug) => ({ slug, display: slug })),
        };
      }
      return {};
    });
  },
};
