/**
 * Onboarding/FirstRunOnboarding — the required Models step of onboarding
 * (`App` renders it after a repository exists but before provider + model roles
 * are set). It walks a brand-new user through two substeps:
 *
 *   1. Connect a provider (Codex OAuth card + API-key form),
 *   2. Assign one primary model to Plan / Code / Review.
 *
 * Its data comes from four `userConfigKeys`-keyed `useQuery` calls
 * (`provider_connected`, `provider_catalog`, `user_settings_get`, and — inside
 * `OnboardingModelSetup` — `provider_models_connected`). We seed those cache
 * entries directly (as `RepositoriesPage`/`ModelSection` do) so no MCP responder
 * is required. The presence of a connected provider decides whether the sheet
 * opens on the Connect substep or resumes straight on role assignment.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";

import { FirstRunOnboarding } from "./FirstRunOnboarding";
import { userConfigKeys } from "@/components/userConfig/userConfigKeys";
import {
  SELF_TARGET,
  type CatalogProvider,
  type ConnectedProvider,
  type UserModel,
  type UserModelSelection,
} from "@/api/userConfig";

// ── Fixtures ────────────────────────────────────────────────────────────────

const PROVIDER_BASE = {
  base_url: "",
  builtin_id: "",
  connected: true,
  connection_methods: ["api_key"],
  docs_url: "https://docs.example.com",
  env_vars: ["API_KEY"],
  goose_provider_id: "",
  id: "anthropic",
  is_openai_compatible: false,
  name: "Anthropic",
  npm: "",
  oauth_keys: [],
  oauth_supported: false,
};

function connectedProvider(
  over: Partial<ConnectedProvider> & { id: string; name: string },
): ConnectedProvider {
  return { ...PROVIDER_BASE, ...over } as ConnectedProvider;
}

function catalogProvider(
  over: Partial<CatalogProvider> & { id: string; name: string },
): CatalogProvider {
  return { ...PROVIDER_BASE, ...over } as CatalogProvider;
}

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

const catalog: CatalogProvider[] = [
  catalogProvider({ id: "anthropic", name: "Anthropic", env_vars: ["ANTHROPIC_API_KEY"] }),
  catalogProvider({ id: "openai", name: "OpenAI", env_vars: ["OPENAI_API_KEY"], oauth_supported: true }),
  catalogProvider({ id: "google", name: "Google AI", env_vars: ["GEMINI_API_KEY"] }),
];

const connectedModels: UserModel[] = [
  model({ id: "anthropic/claude-sonnet-4-6", name: "Claude Sonnet 4.6", provider_id: "anthropic", recommended: true }),
  model({ id: "anthropic/claude-opus-4-6", name: "Claude Opus 4.6", provider_id: "anthropic", recommended: true, reasoning: true }),
  model({ id: "openai/gpt-4o", name: "GPT-4o", provider_id: "openai" }),
];

function selection(over: Partial<UserModelSelection> = {}): UserModelSelection {
  return {
    lanes: { plan: [], implement: [], review: [] },
    maxSessions: {},
    diverseReview: true,
    diverseRefinement: true,
    ...over,
  };
}

interface SeedArgs {
  providers: ConnectedProvider[];
  catalog: CatalogProvider[];
  models: UserModel[];
  selection: UserModelSelection;
}

function seededClient({ providers, catalog, models, selection }: SeedArgs): QueryClient {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  qc.setQueryData(userConfigKeys.connectedProviders(SELF_TARGET), providers);
  qc.setQueryData(userConfigKeys.catalog(SELF_TARGET), catalog);
  qc.setQueryData(userConfigKeys.connectedModels(SELF_TARGET), models);
  qc.setQueryData(userConfigKeys.modelSelection(SELF_TARGET), selection);
  return qc;
}

// ── Harness ───────────────────────────────────────────────────────────────

const meta = {
  title: "Onboarding/FirstRunOnboarding",
  component: FirstRunOnboarding,
  parameters: { layout: "fullscreen" },
  args: { onFinished: () => {} },
} satisfies Meta<typeof FirstRunOnboarding>;

export default meta;
type Story = StoryObj<typeof meta>;

function withClient(seed: SeedArgs) {
  return [
    (Story: React.ComponentType) => (
      <QueryClientProvider client={seededClient(seed)}>
        <MemoryRouter>
          <Story />
        </MemoryRouter>
      </QueryClientProvider>
    ),
  ];
}

/**
 * No provider connected yet → the sheet opens on the Connect substep: the Codex
 * OAuth card plus the API-key form (driven by the seeded provider catalog).
 */
export const ConnectStep: Story = {
  decorators: withClient({
    providers: [],
    catalog,
    models: [],
    selection: selection(),
  }),
};

/**
 * A provider is connected → the sheet resumes on role assignment, with three
 * connected models available to map onto Plan / Code / Review.
 */
export const ModelsStep: Story = {
  decorators: withClient({
    providers: [connectedProvider({ id: "anthropic", name: "Anthropic" })],
    catalog,
    models: connectedModels,
    selection: selection({
      lanes: {
        plan: ["anthropic/claude-opus-4-6"],
        implement: ["anthropic/claude-sonnet-4-6"],
        review: ["openai/gpt-4o"],
      },
      maxSessions: {
        "anthropic/claude-opus-4-6": 1,
        "anthropic/claude-sonnet-4-6": 2,
        "openai/gpt-4o": 1,
      },
    }),
  }),
};

/**
 * Org AI policy owns the lanes and they are fully assigned → the read-only
 * "Managed by your organization" panel with a Continue action.
 */
export const ManagedByOrg: Story = {
  decorators: withClient({
    providers: [connectedProvider({ id: "anthropic", name: "Anthropic" })],
    catalog,
    models: connectedModels,
    selection: selection({
      laneLocked: true,
      lanes: {
        plan: ["anthropic/claude-opus-4-6"],
        implement: ["anthropic/claude-sonnet-4-6"],
        review: ["openai/gpt-4o"],
      },
    }),
  }),
};

/**
 * Org policy owns the lanes but hasn't assigned them → the "needs an
 * administrator" warning blocks the user from continuing.
 */
export const OrgPolicyNeedsAdmin: Story = {
  decorators: withClient({
    providers: [connectedProvider({ id: "anthropic", name: "Anthropic" })],
    catalog,
    models: connectedModels,
    selection: selection({ laneLocked: true }),
  }),
};
