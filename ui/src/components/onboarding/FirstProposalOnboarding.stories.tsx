/**
 * The final showroom step: explain how a proposal moves from an idea to
 * executable work, then create the repository's first safe draft.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";

import type { Project } from "@/api/server";

import { FirstProposalOnboarding } from "./FirstProposalOnboarding";

const project = {
  id: "project-conversational-cms",
  name: "conversational-cms-platform",
  github_owner: "CassioRoos",
  github_repo: "conversational-cms-platform",
} as Project;

const meta = {
  title: "Onboarding/FirstProposalOnboarding",
  component: FirstProposalOnboarding,
  parameters: { layout: "fullscreen" },
  args: { project, onFinished: () => {} },
  decorators: [
    (Story) => {
      const queryClient = new QueryClient({
        defaultOptions: {
          queries: { retry: false },
          mutations: { retry: false },
        },
      });
      return (
        <QueryClientProvider client={queryClient}>
          <MemoryRouter>
            <Story />
          </MemoryRouter>
        </QueryClientProvider>
      );
    },
  ],
} satisfies Meta<typeof FirstProposalOnboarding>;

export default meta;
type Story = StoryObj<typeof meta>;

export const DraftForm: Story = {};
