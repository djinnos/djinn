/**
 * Proposals/FeedbackThread — the human feedback thread on a proposal detail
 * (exported from `ProposalsPage`). Lists unresolved comments (human + AI
 * authors) with "Address with djinn" / "Dismiss" actions, and a collapsible
 * "resolved" section noting the revision that addressed each. `canEdit` is a
 * plain prop here (unlike the page, which derives it from the auth user), so
 * the action buttons render. Reads org users via TanStack Query (seeded) and
 * uses `useStartProposalChat`, which needs a router.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { FeedbackThread } from "./ProposalsPage";
import {
  feedback,
  richProposal,
  users,
} from "@/components/proposals/proposalStoryFixtures";

function makeClient(): QueryClient {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  qc.setQueryData(["users", "list"], users);
  return qc;
}

const meta = {
  title: "Proposals/FeedbackThread",
  component: FeedbackThread,
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
    proposal: richProposal,
    onChanged: () => {},
  },
} satisfies Meta<typeof FeedbackThread>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * Two open comments (a human reviewer and an AI reviewer) with the editor
 * actions, plus one resolved comment behind the "Show resolved" toggle.
 */
export const Mixed: Story = {
  args: {
    feedback,
    canEdit: true,
  },
};

/** No open feedback → the empty-thread guidance copy. */
export const Empty: Story = {
  args: {
    feedback: [],
    canEdit: true,
  },
};
