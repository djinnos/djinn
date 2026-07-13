import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { ModelSection } from "@/components/userConfig/ModelSection";
import { userConfigKeys } from "@/components/userConfig/userConfigKeys";
import {
  type UserModel,
  type UserModelSelection,
  SELF_TARGET,
} from "@/api/userConfig";

/**
 * Isolated stories for the real `ModelSection` — the Model Roles tab body. Its
 * two `useQuery` calls (`provider_models_connected` + `user_settings_get`, keyed
 * under `userConfigKeys`) are seeded so the per-role lane editor renders with a
 * realistic selection and both cross-model toggles active.
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
  model({ id: "anthropic/claude-opus-4-6", name: "Claude Opus 4.6", provider_id: "anthropic", recommended: true, reasoning: true }),
  model({ id: "openai/gpt-4o", name: "GPT-4o", provider_id: "openai" }),
  model({ id: "openai/gpt-4.1", name: "GPT-4.1", provider_id: "openai" }),
  model({ id: "google/gemini-2.5-pro", name: "Gemini 2.5 Pro", provider_id: "google", recommended: true }),
];

const selection: UserModelSelection = {
  lanes: {
    plan: ["anthropic/claude-opus-4-6", "google/gemini-2.5-pro"],
    implement: ["anthropic/claude-sonnet-4-6"],
    review: ["openai/gpt-4o"],
  },
  maxSessions: {
    "anthropic/claude-opus-4-6": 2,
    "google/gemini-2.5-pro": 1,
    "anthropic/claude-sonnet-4-6": 3,
    "openai/gpt-4o": 2,
  },
  diverseReview: true,
  diverseRefinement: true,
};

function client(models: UserModel[], sel: UserModelSelection): QueryClient {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  qc.setQueryData(userConfigKeys.connectedModels(SELF_TARGET), models);
  qc.setQueryData(userConfigKeys.modelSelection(SELF_TARGET), sel);
  return qc;
}

const meta = {
  title: "Settings/Model Roles",
  component: ModelSection,
  parameters: { layout: "padded" },
  args: { targetId: SELF_TARGET },
} satisfies Meta<typeof ModelSection>;

export default meta;

type Story = StoryObj<typeof meta>;

/** Populated lanes across plan / implement / review with per-model caps. */
export const Populated: Story = {
  decorators: [
    (Story) => (
      <QueryClientProvider client={client(connectedModels, selection)}>
        <MemoryRouter>
          <div className="mx-auto max-w-3xl p-6">
            <Story />
          </div>
        </MemoryRouter>
      </QueryClientProvider>
    ),
  ],
};

/** No connected models — the "connect a provider first" empty state. */
export const NoModels: Story = {
  decorators: [
    (Story) => (
      <QueryClientProvider
        client={client([], { lanes: { plan: [], implement: [], review: [] }, maxSessions: {}, diverseReview: true, diverseRefinement: true })}
      >
        <MemoryRouter>
          <div className="mx-auto max-w-3xl p-6">
            <Story />
          </div>
        </MemoryRouter>
      </QueryClientProvider>
    ),
  ],
};
