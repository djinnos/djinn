/**
 * Galaxy storybook — the look-and-feel gate for the 3D galaxy view
 * (proposal lmkv) before it wires into the code-graph page.
 *
 * Fixtures are seeded procedural "codebases" (see galaxyFixture.ts):
 * packages → files → symbols with preferential-attachment call edges, so
 * degree distribution — and therefore the stellar color spread — matches
 * what a real snapshot produces. Everything is deterministic; no network,
 * no MCP mock needed (the component is fed data directly).
 *
 * Judge with: Medium for the default look, Large for density compensation
 * at ~20k nodes, ComplexityHeat for the heat mode, Labels for the sprite
 * pass. Click a star to fly to its neighborhood; click space to fly back;
 * idle 20s for the ambient auto-rotate.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";

import { GalaxyCanvas } from "./GalaxyCanvas";
import { FIXTURE_PRESETS, makeGalaxyFixture } from "./galaxyFixture";
import { DEFAULT_GALAXY_DISPLAY, type GalaxyDisplay } from "./galaxyTypes";

// Module-level: generated once, reused across stories/args changes. The
// xlarge (~50k node) fixture is generated lazily so the other stories
// don't pay for it.
const SMALL = makeGalaxyFixture(FIXTURE_PRESETS.small);
const MEDIUM = makeGalaxyFixture(FIXTURE_PRESETS.medium);
const LARGE = makeGalaxyFixture(FIXTURE_PRESETS.large);
let xlargeCache: ReturnType<typeof makeGalaxyFixture> | null = null;
function xlarge() {
  xlargeCache ??= makeGalaxyFixture(FIXTURE_PRESETS.xlarge);
  return xlargeCache;
}

interface GalaxyArgs {
  fixture: "small" | "medium" | "large" | "xlarge";
  colorMode: "group" | "heat";
  showLabels: boolean;
  edgeBrightness: number;
  nodeGlow: number;
  bloom: number;
}

function fixtureData(name: GalaxyArgs["fixture"]) {
  if (name === "xlarge") return xlarge();
  return { small: SMALL, medium: MEDIUM, large: LARGE }[name];
}

function GalaxyHarness({
  fixture,
  colorMode,
  showLabels,
  edgeBrightness,
  nodeGlow,
  bloom,
}: GalaxyArgs) {
  const display: GalaxyDisplay = { edgeBrightness, nodeGlow, bloom };
  const data = fixtureData(fixture);
  return (
    <div className="h-screen w-full">
      <GalaxyCanvas
        data={data}
        colorMode={colorMode}
        showLabels={showLabels}
        display={display}
        title={`djinn / ${fixture} fixture`}
      />
    </div>
  );
}

const meta = {
  title: "CodeGraph/Galaxy",
  component: GalaxyHarness,
  parameters: { layout: "fullscreen" },
  args: {
    fixture: "medium",
    colorMode: "group",
    showLabels: false,
    edgeBrightness: DEFAULT_GALAXY_DISPLAY.edgeBrightness,
    nodeGlow: DEFAULT_GALAXY_DISPLAY.nodeGlow,
    bloom: DEFAULT_GALAXY_DISPLAY.bloom,
  },
  argTypes: {
    fixture: { control: "radio", options: ["small", "medium", "large", "xlarge"] },
    colorMode: { control: "radio", options: ["group", "heat"] },
    edgeBrightness: { control: { type: "range", min: 0.1, max: 3, step: 0.05 } },
    nodeGlow: { control: { type: "range", min: 0, max: 2, step: 0.05 } },
    bloom: { control: { type: "range", min: 0, max: 2, step: 0.05 } },
  },
} satisfies Meta<typeof GalaxyHarness>;

export default meta;
type Story = StoryObj<typeof meta>;

/** The default look: ~3–4k nodes, per-crate colors. */
export const Medium: Story = {};

/** Density compensation at scale: ~20k nodes / ~45k edges, no white blob. */
export const Large: Story = {
  args: { fixture: "large" },
};

/** The uncapped regime: ~50k nodes. Interaction off, pure spectacle. */
export const XLarge50k: Story = {
  args: { fixture: "xlarge" },
};

/** Small graph — verifies the look doesn't fall apart under sparse data. */
export const Small: Story = {
  args: { fixture: "small" },
};

/** Cognitive-complexity heat mode: hot functions burn red, types/files mute. */
export const ComplexityHeat: Story = {
  args: { colorMode: "heat" },
};

/** Label sprites for the top nodes by visual weight. */
export const Labels: Story = {
  args: { showLabels: true },
};
