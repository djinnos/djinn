import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { AiPolicyTab } from "@/components/settings/AiPolicyTab";
import { userConfigKeys } from "@/components/userConfig/userConfigKeys";
import { type UserModel, SELF_TARGET } from "@/api/userConfig";
import type { OrgPolicy } from "@/api/orgPolicy";

/**
 * Isolated stories for the real admin-only `AiPolicyTab`. Its `org_policy_get`
 * query (`["org-policy"]`) plus the org-default lane editor's
 * `provider_models_connected` query (keyed under `userConfigKeys`) are seeded so
 * the subscription residency table, org-default lanes, and lock toggle all
 * render populated.
 */

function model(
  over: Partial<UserModel> & { id: string; name: string; provider_id: string },
): UserModel {
  return {
    attachment: false,
    context_window: 200_000,
    output_limit: 8_192,
    pricing: {
      cache_read_per_million: 0.3,
      cache_write_per_million: 3.75,
      input_per_million: 3,
      output_per_million: 15,
    },
    reasoning: false,
    recommended: false,
    tool_call: true,
    ...over,
  };
}

const connectedModels: UserModel[] = [
  model({ id: "anthropic/claude-sonnet-4-6", name: "Claude Sonnet 4.6", provider_id: "anthropic", recommended: true }),
  model({ id: "anthropic/claude-opus-4-6", name: "Claude Opus 4.6", provider_id: "anthropic", recommended: true }),
  model({ id: "openai/gpt-4o", name: "GPT-4o", provider_id: "openai" }),
  model({ id: "google/gemini-2.5-pro", name: "Gemini 2.5 Pro", provider_id: "google", recommended: true }),
];

const orgPolicy: OrgPolicy = {
  subscriptions: [
    { id: "chatgpt_codex", name: "ChatGPT / Codex", jurisdiction: "us", blocked: false },
    { id: "opencode", name: "OpenCode", jurisdiction: "us", blocked: false },
    { id: "mistral-coding-plan", name: "Mistral Coding Plan", jurisdiction: "eu", blocked: false },
    { id: "kimi-for-coding", name: "Kimi for Coding", jurisdiction: "cn", blocked: false },
    { id: "zai-coding-plan", name: "Z.AI Coding Plan", jurisdiction: "cn", blocked: true },
    { id: "minimax-coding-plan", name: "MiniMax Coding Plan", jurisdiction: "cn", blocked: false },
  ],
  blockedSubscriptions: ["zai-coding-plan"],
  defaultLanes: {
    plan: ["anthropic/claude-opus-4-6"],
    implement: ["anthropic/claude-sonnet-4-6"],
    review: ["openai/gpt-4o"],
  },
  lockLevel: "flexible",
};

function client(policy: OrgPolicy): QueryClient {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  qc.setQueryData(["org-policy"], policy);
  qc.setQueryData(userConfigKeys.connectedModels(SELF_TARGET), connectedModels);
  return qc;
}

const meta = {
  title: "Settings/AI Policy",
  component: AiPolicyTab,
  parameters: { layout: "padded" },
} satisfies Meta<typeof AiPolicyTab>;

export default meta;

type Story = StoryObj<typeof meta>;

/** Residency table (with a blocked CN plan), org-default lanes, and lock off. */
export const Populated: Story = {
  decorators: [
    (Story) => (
      <QueryClientProvider client={client(orgPolicy)}>
        <MemoryRouter>
          <div className="mx-auto max-w-3xl p-6">
            <Story />
          </div>
        </MemoryRouter>
      </QueryClientProvider>
    ),
  ],
};

/** Lane lock ON — the org default is authoritative and members can't override. */
export const Locked: Story = {
  decorators: [
    (Story) => (
      <QueryClientProvider client={client({ ...orgPolicy, lockLevel: "locked" })}>
        <MemoryRouter>
          <div className="mx-auto max-w-3xl p-6">
            <Story />
          </div>
        </MemoryRouter>
      </QueryClientProvider>
    ),
  ],
};
