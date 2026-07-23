/**
 * Onboarding/OnboardingGateStatus — the interstitial `App` renders while the
 * project/model/server readiness gates are still resolving, and the retryable
 * error screen it swaps to when the setup check itself fails (connection or
 * project-gate error).
 */

import type { Meta, StoryObj } from "@storybook/react-vite";

import { OnboardingGateStatus } from "./OnboardingGateStatus";

const meta = {
  title: "Onboarding/OnboardingGateStatus",
  component: OnboardingGateStatus,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof OnboardingGateStatus>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Default spinner state — "Checking your setup…". */
export const Checking: Story = {};

/** The setup check failed and offers a retry. */
export const ConnectionError: Story = {
  args: {
    error: "Could not connect to Djinn",
    onRetry: () => {},
  },
};

/** A project-gate error surfaces the server-supplied message. */
export const ProjectError: Story = {
  args: {
    error: "Failed to load project setup status: 503 Service Unavailable",
    onRetry: () => {},
  },
};
