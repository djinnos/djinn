/**
 * Proposals/ProposalKickoff — the graduation control and graduated-epic list.
 *
 * The component has two surfaces: an "Ready to build" kick-off picker (shown on
 * an approved proposal to engineers/admins) and, once graduated, the list of
 * spawned epics. The kick-off picker is gated on `canKickoff`, which needs the
 * authenticated user — null in Storybook — so this story covers the graduated
 * state, which renders from `detail.epics` regardless of auth.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { ProposalKickoff } from "./ProposalKickoff";
import { graduatedDetail, users } from "./proposalStoryFixtures";

function makeClient(): QueryClient {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  qc.setQueryData(["users", "list"], users);
  return qc;
}

const meta = {
  title: "Proposals/ProposalKickoff",
  component: ProposalKickoff,
  parameters: { layout: "padded" },
  decorators: [
    (Story) => (
      <QueryClientProvider client={makeClient()}>
        <MemoryRouter>
          <div className="mx-auto max-w-2xl bg-background p-4 text-foreground">
            <Story />
          </div>
        </MemoryRouter>
      </QueryClientProvider>
    ),
  ],
  args: {
    onChanged: () => {},
  },
} satisfies Meta<typeof ProposalKickoff>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * A graduated proposal: the two spawned epics with their status badges, the
 * build owner, and the reconcile state (one reconciled, one needs-reconcile).
 */
export const GraduatedEpics: Story = {
  args: {
    detail: graduatedDetail,
  },
};
