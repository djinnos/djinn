/**
 * Proposals/ProposalDebateTrail — the round-by-round tribunal timeline. Groups
 * the raw debate trail by round (objections, rebuttals, verdicts), summarising
 * each round on one line and expanding to the individual entries. The latest
 * round starts open; earlier rounds collapse. Renders purely from the `trail`
 * prop — no providers required.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";

import { ProposalDebateTrail } from "./ProposalDebateTrail";
import { debateTrail } from "./proposalStoryFixtures";

const meta = {
  title: "Proposals/ProposalDebateTrail",
  component: ProposalDebateTrail,
  parameters: { layout: "padded" },
  decorators: [
    (Story) => (
      <div className="mx-auto max-w-3xl bg-background p-4 text-foreground">
        <Story />
      </div>
    ),
  ],
} satisfies Meta<typeof ProposalDebateTrail>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * Two rounds: round 1 has a resolved blocking objection, a rebuttal, and a
 * needs-work verdict; round 2 has an open objection and an approve verdict.
 */
export const Rounds: Story = {
  args: { trail: debateTrail },
};

/** Empty trail → the "No debate entries yet." placeholder. */
export const Empty: Story = {
  args: { trail: [] },
};
