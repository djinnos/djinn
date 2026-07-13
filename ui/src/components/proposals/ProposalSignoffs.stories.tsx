/**
 * Proposals/ProposalSignoffs — the two-kind sign-off panel (scope / technical).
 *
 * Each kind lists its signers as badges (stale signers flagged), and a proposal
 * is approved once both scope and technical are fresh. The sign-off / withdraw
 * buttons are gated on the authenticated user (null in Storybook), so these
 * stories exercise the sign-off *states* — none, partial, and complete with a
 * stale re-approval — which render from `detail.signoffs`.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { ProposalSignoffs } from "./ProposalSignoffs";
import {
  richDetail,
  signoffsComplete,
  signoffsNone,
  signoffsPartial,
  users,
} from "./proposalStoryFixtures";
import type { ProposalDetail } from "@/lib/proposalQueries";
import type { ProposalSignoff } from "@/api/types";

function makeClient(): QueryClient {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  qc.setQueryData(["users", "list"], users);
  return qc;
}

const detailWith = (signoffs: ProposalSignoff[]): ProposalDetail => ({
  ...richDetail,
  signoffs,
});

const meta = {
  title: "Proposals/ProposalSignoffs",
  component: ProposalSignoffs,
  parameters: { layout: "padded" },
  decorators: [
    (Story) => (
      <QueryClientProvider client={makeClient()}>
        <div className="mx-auto max-w-2xl bg-background p-4 text-foreground">
          <Story />
        </div>
      </QueryClientProvider>
    ),
  ],
  args: {
    onChanged: () => {},
  },
} satisfies Meta<typeof ProposalSignoffs>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Neither kind signed off yet. */
export const None: Story = {
  args: { detail: detailWith(signoffsNone) },
};

/** Scope signed off; technical still outstanding. */
export const Partial: Story = {
  args: { detail: detailWith(signoffsPartial) },
};

/** Both kinds signed off — technical is stale (signed against an older rev). */
export const Complete: Story = {
  args: { detail: detailWith(signoffsComplete) },
};
