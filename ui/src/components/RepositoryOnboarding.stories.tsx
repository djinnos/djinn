/**
 * Onboarding/RepositoryOnboarding — the first required step: no repository has
 * been added to the deployment yet. A single "Browse repositories" CTA opens
 * the GitHub repo picker (`AddProjectFromGithubDialog`, whose queries stay
 * `enabled: open`, so the closed dialog makes no calls here). The component
 * reads only the zustand `projectGateStore`, so no MCP responder is needed —
 * we just provide a `QueryClient` for the dialog's `useQuery` hooks.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { RepositoryOnboarding } from "./RepositoryOnboarding";

const meta = {
  title: "Onboarding/RepositoryOnboarding",
  component: RepositoryOnboarding,
  parameters: { layout: "fullscreen" },
  decorators: [
    (Story) => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false, staleTime: Infinity } },
      });
      return (
        <QueryClientProvider client={queryClient}>
          <Story />
        </QueryClientProvider>
      );
    },
  ],
} satisfies Meta<typeof RepositoryOnboarding>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};
