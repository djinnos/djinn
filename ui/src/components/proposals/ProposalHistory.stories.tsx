/**
 * Proposals/ProposalHistory — the spec revision timeline. Every material edit
 * appends a full snapshot; a contiguous tribunal run collapses into one
 * "Refined via tribunal" row whose diff is the pre-refinement snapshot → the
 * converged head. Status transitions appear as their own rows. Reads org users
 * via TanStack Query (seeded); expandable rows show a line-level DiffView.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { ProposalHistory } from "./ProposalHistory";
import { richDetail, users } from "./proposalStoryFixtures";

function makeClient(): QueryClient {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  qc.setQueryData(["users", "list"], users);
  return qc;
}

const meta = {
  title: "Proposals/ProposalHistory",
  component: ProposalHistory,
  parameters: { layout: "padded" },
  decorators: [
    (Story) => (
      <QueryClientProvider client={makeClient()}>
        <div className="mx-auto max-w-3xl bg-background p-4 text-foreground">
          <Story />
        </div>
      </QueryClientProvider>
    ),
  ],
} satisfies Meta<typeof ProposalHistory>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * A seed revision, a two-round tribunal run collapsed into one row, and a
 * draft → in-review status transition. Click a row to expand its diff.
 */
export const WithRevisions: Story = {
  args: { detail: richDetail },
};
