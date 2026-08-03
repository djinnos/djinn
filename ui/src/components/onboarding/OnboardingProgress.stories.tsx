/**
 * Onboarding/OnboardingProgress — the 4-step stepper (Repository → Models →
 * Environment → First proposal) shown at the top of every full-screen
 * onboarding gate.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";

import { OnboardingProgress } from "./OnboardingProgress";

const meta = {
  title: "Onboarding/OnboardingProgress",
  component: OnboardingProgress,
  parameters: { layout: "centered" },
  decorators: [
    (Story) => (
      <div className="w-full max-w-xl bg-background p-8 text-foreground">
        <Story />
      </div>
    ),
  ],
} satisfies Meta<typeof OnboardingProgress>;

export default meta;
type Story = StoryObj<typeof meta>;

export const RepositoryStep: Story = { args: { current: "repository" } };
export const ModelsStep: Story = { args: { current: "models" } };
export const EnvironmentStep: Story = { args: { current: "environment" } };
export const FirstProposalStep: Story = { args: { current: "proposal" } };
export const Complete: Story = { args: { current: "proposal", complete: true } };
