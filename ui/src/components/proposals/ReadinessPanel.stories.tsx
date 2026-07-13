/**
 * Proposals/ReadinessPanel — the readiness gate rendered as a per-condition
 * checklist: Definition of Ready, judge verdict, unresolved blocking debate
 * entries, and the evidence spike. The overall Ready/Blocked badge comes
 * straight from `gate_status.ready`; the panel renders backend status only.
 *
 * `callMcpTool` is invoked only on the resolve / override actions, so the
 * stories render entirely from props.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";

import { ReadinessPanel } from "./ReadinessPanel";
import {
  debateTrail,
  gateBlocked,
  gateReady,
  refinementAwaitingReview,
  refinementStopped,
} from "./proposalStoryFixtures";

const meta = {
  title: "Proposals/ReadinessPanel",
  component: ReadinessPanel,
  parameters: { layout: "padded" },
  decorators: [
    (Story) => (
      <div className="mx-auto max-w-2xl bg-background p-4 text-foreground">
        <Story />
      </div>
    ),
  ],
  args: {
    proposalId: "prop-refine",
    debateTrail,
    onChanged: () => {},
  },
} satisfies Meta<typeof ReadinessPanel>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * Blocked: DoR has two failing checks, the judge verdict is needs-work, and one
 * blocking debate entry is unresolved (expand "show" to see it).
 */
export const PartiallyMet: Story = {
  args: {
    gateStatus: gateBlocked,
    refinement: refinementAwaitingReview,
  },
};

/** Ready: every gate row clears and the badge flips to Ready. */
export const FullyMet: Story = {
  args: {
    gateStatus: gateReady,
    refinement: refinementStopped,
  },
};
