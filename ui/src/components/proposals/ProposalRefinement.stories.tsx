/**
 * Proposals/ProposalRefinement — the unified Tribunal section: refinement
 * kick-off, the running/stopped status ribbon, and the converged review card
 * (judge verdict / spec diff / round-by-round debate trail behind tabs, plus
 * the human accept / another-round / reject actions).
 *
 * The component reads the org-users list via TanStack Query (seeded into the
 * per-file QueryClient) and calls `callMcpTool` only on user action, so the
 * stories render purely from props. `useAuthUser()` is null in Storybook, which
 * only affects the pre-selected owner in the kick-off picker.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { ProposalRefinement } from "./ProposalRefinement";
import {
  debateTrail,
  gateBlocked,
  refinementAwaitingReview,
  refinementRunning,
  refinementStopped,
  revisions,
  users,
} from "./proposalStoryFixtures";

function makeClient(): QueryClient {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  qc.setQueryData(["users", "list"], users);
  return qc;
}

const meta = {
  title: "Proposals/ProposalRefinement",
  component: ProposalRefinement,
  parameters: { layout: "padded" },
  decorators: [
    (Story) => (
      <QueryClientProvider client={makeClient()}>
        <MemoryRouter>
          <div className="mx-auto max-w-3xl bg-background p-4 text-foreground">
            <Story />
          </div>
        </MemoryRouter>
      </QueryClientProvider>
    ),
  ],
  args: {
    proposalId: "prop-refine",
    debateTrail,
    revisions,
    onChanged: () => {},
  },
} satisfies Meta<typeof ProposalRefinement>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Not yet started — the kick-off affordance with the owner picker. */
export const Kickoff: Story = {
  args: {
    status: null,
    gateStatus: null,
    canStart: true,
  },
};

/** A tribunal round running autonomously; no human review yet. */
export const Running: Story = {
  args: {
    status: refinementRunning,
    gateStatus: gateBlocked,
    canStart: false,
  },
};

/**
 * Converged and parked for review — the judge verdict, the spec diff, and the
 * debate trail are behind tabs, with accept / another-round / reject actions.
 */
export const AwaitingReview: Story = {
  args: {
    status: refinementAwaitingReview,
    gateStatus: gateBlocked,
    canStart: false,
  },
};

/** Stopped after the adversary went dry — the restart affordance. */
export const Stopped: Story = {
  args: {
    status: refinementStopped,
    gateStatus: gateBlocked,
    canStart: false,
  },
};
