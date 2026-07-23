import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { within, userEvent } from "storybook/test";

import { SettingsPage } from "@/pages/SettingsPage";
import { AuthGate } from "@/components/AuthGate";
import { installAuthFetchStub, setStoryIsAdmin } from "@/storybook-mocks/authFetchStub";
import { userConfigKeys } from "@/components/userConfig/userConfigKeys";
import {
  type CatalogProvider,
  type ConnectedProvider,
  type UserModel,
  type UserModelSelection,
  SELF_TARGET,
} from "@/api/userConfig";
import type { OrgPolicy } from "@/api/orgPolicy";

/* -------------------------------------------------------------------------- */
/*  Real-component stories for the routed /settings page.                      */
/*                                                                            */
/*  These render the ACTUAL `SettingsPage` (Connections / Model Roles /        */
/*  Preferences / AI Policy tabs), not a hand-built replica. Data is seeded    */
/*  into a per-story TanStack Query cache (retry:false, staleTime:Infinity) so */
/*  the components' `useQuery` calls resolve from the fixtures instead of      */
/*  hitting a live backend — the same pattern as CodeGraphPage.stories.        */
/*                                                                            */
/*  Two hooks the page depends on fire imperative (non-query) fetches that     */
/*  can't be seeded through the cache:                                         */
/*   - `useServerHealth` polls `GET /health` to gate the whole page, and       */
/*   - `AuthGate` reads `/auth/me` + `/setup/status` + `/auth/config` to prove */
/*     the caller (and whether they're an admin, which reveals AI Policy).     */
/*  A tiny `window.fetch` shim below answers exactly those four endpoints and  */
/*  delegates everything else (including the MCP transport the Preferences tab */
/*  uses) to the real fetch, which fails into a graceful inline-error state.   */
/* -------------------------------------------------------------------------- */

// Auth endpoints are served by the shared Storybook auth stub.
installAuthFetchStub();

/* -------------------------------- fixtures -------------------------------- */

function connectedProvider(
  over: Partial<ConnectedProvider> & { id: string; name: string },
): ConnectedProvider {
  return {
    base_url: "",
    builtin_id: over.id,
    connected: true,
    connection_methods: ["api_key"],
    docs_url: "",
    env_vars: [],
    goose_provider_id: over.id,
    is_openai_compatible: false,
    npm: "",
    oauth_keys: [],
    oauth_supported: false,
    ...over,
  };
}

const connectedProviders: ConnectedProvider[] = [
  // openai-via-oauth ⇒ ChatGPT/Codex is signed in (rendered by the Codex row).
  connectedProvider({
    id: "openai",
    name: "OpenAI",
    connection_methods: ["oauth"],
    env_vars: ["OPENAI_API_KEY"],
  }),
  // Personal subscriptions → "Your subscriptions" bucket.
  connectedProvider({
    id: "kimi-for-coding",
    name: "Kimi for Coding",
    env_vars: ["MOONSHOT_API_KEY"],
  }),
  // Two Zhipu plans share ZHIPU_API_KEY → collapse into one "Z.AI / Zhipu" card.
  connectedProvider({
    id: "zai-coding-plan",
    name: "Z.AI Coding Plan",
    env_vars: ["ZHIPU_API_KEY"],
  }),
  connectedProvider({
    id: "zhipuai-coding-plan",
    name: "Zhipu AI Coding Plan",
    env_vars: ["ZHIPU_API_KEY"],
  }),
  // Org-provisioned API keys → "Provided by your org" bucket (locked).
  connectedProvider({
    id: "anthropic",
    name: "Anthropic",
    env_vars: ["ANTHROPIC_API_KEY"],
  }),
  connectedProvider({
    id: "google",
    name: "Google Gemini",
    env_vars: ["GEMINI_API_KEY"],
  }),
];

const catalog: CatalogProvider[] = [
  connectedProvider({ id: "anthropic", name: "Anthropic", connected: false, connection_methods: [], env_vars: ["ANTHROPIC_API_KEY"] }),
  connectedProvider({ id: "openai", name: "OpenAI", connected: false, connection_methods: [], env_vars: ["OPENAI_API_KEY"] }),
  connectedProvider({ id: "google", name: "Google Gemini", connected: false, connection_methods: [], env_vars: ["GEMINI_API_KEY"] }),
  connectedProvider({ id: "mistral", name: "Mistral", connected: false, connection_methods: [], env_vars: ["MISTRAL_API_KEY"] }),
  connectedProvider({ id: "groq", name: "Groq", connected: false, connection_methods: [], env_vars: ["GROQ_API_KEY"] }),
];

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

const modelSelection: UserModelSelection = {
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

/** A fresh QueryClient seeded with every fixture the four tabs read. */
function seededClient(): QueryClient {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  client.setQueryData(userConfigKeys.connectedProviders(SELF_TARGET), connectedProviders);
  client.setQueryData(userConfigKeys.catalog(SELF_TARGET), catalog);
  client.setQueryData(userConfigKeys.connectedModels(SELF_TARGET), connectedModels);
  client.setQueryData(userConfigKeys.modelSelection(SELF_TARGET), modelSelection);
  client.setQueryData(["org-policy"], orgPolicy);
  return client;
}

const meta = {
  title: "Settings/SettingsPage",
  component: SettingsPage,
  parameters: { layout: "fullscreen" },
  decorators: [
    (Story, ctx) => {
      setStoryIsAdmin((ctx.parameters?.isAdmin as boolean | undefined) ?? true);
      return (
        // Key by story id so switching stories fully remounts the tree —
        // otherwise AuthGate's cached /auth/me (staleTime) survives the
        // switch and the admin flag flip never takes effect.
        <QueryClientProvider key={ctx.id} client={seededClient()}>
          <MemoryRouter initialEntries={["/settings"]}>
            <AuthGate>
              <div className="h-screen">
                <Story />
              </div>
            </AuthGate>
          </MemoryRouter>
        </QueryClientProvider>
      );
    },
  ],
} satisfies Meta<typeof SettingsPage>;

export default meta;

type Story = StoryObj<typeof meta>;

/** Default landing tab — connected subscriptions + org-provided API keys. */
export const Connections: Story = {};

/** Model Roles tab — per-role (plan / implement / review) model lanes. */
export const ModelRoles: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await userEvent.click(await canvas.findByRole("tab", { name: "Model Roles" }));
  },
};

/**
 * Preferences tab — the real PreferencesTab. Its `useUserSettings` hook fires an
 * imperative MCP call that can't be cache-seeded, so with no live server it
 * settles into its graceful inline-error state rather than crashing.
 */
export const Preferences: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await userEvent.click(await canvas.findByRole("tab", { name: "Preferences" }));
  },
};

/** AI Policy tab (admin-only) — subscription residency controls + org lanes. */
export const AiPolicy: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await userEvent.click(await canvas.findByRole("tab", { name: "AI Policy" }));
  },
};

/** Non-admin caller — the AI Policy tab is hidden entirely. */
export const MemberView: Story = {
  parameters: { isAdmin: false },
};
