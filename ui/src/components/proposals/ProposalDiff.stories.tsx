/**
 * Proposals/ProposalDiff — the collapsible revision-drift diff. Given the full
 * revision list plus a base and head seq, it renders a git-style line diff of
 * the two revisions' markdown snapshots (title + body + acceptance criteria).
 * Rendered open here via `defaultOpen`. No providers required.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";

import { ProposalDiff } from "./ProposalDiff";
import { revisions } from "./proposalStoryFixtures";

const meta = {
  title: "Proposals/ProposalDiff",
  component: ProposalDiff,
  parameters: { layout: "padded" },
  decorators: [
    (Story) => (
      <div className="mx-auto max-w-3xl bg-background p-4 text-foreground">
        <Story />
      </div>
    ),
  ],
  args: {
    revisions,
    defaultOpen: true,
  },
} satisfies Meta<typeof ProposalDiff>;

export default meta;
type Story = StoryObj<typeof meta>;

/** A small spec diff between rev 1 and rev 3 (open by default). */
export const SmallDiff: Story = {
  args: {
    baseSeq: 1,
    headSeq: 3,
  },
};

/** A missing head revision → the graceful "cannot be displayed" message. */
export const MissingRevision: Story = {
  args: {
    baseSeq: 1,
    headSeq: 99,
  },
};
