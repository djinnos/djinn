import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { ConnectionsTab } from "@/components/settings/ConnectionsTab";
import { userConfigKeys } from "@/components/userConfig/userConfigKeys";
import {
  type CatalogProvider,
  type ConnectedProvider,
  SELF_TARGET,
} from "@/api/userConfig";

/**
 * Isolated stories for the real `ConnectionsTab`. The tab's `useQuery` calls
 * (`provider_connected` + `provider_catalog`, keyed under `userConfigKeys`) are
 * satisfied from a seeded TanStack Query cache, so it renders both provider
 * buckets — "Your subscriptions" and "Provided by your org" — without a server.
 */

function provider(
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

const connected: ConnectedProvider[] = [
  provider({ id: "openai", name: "OpenAI", connection_methods: ["oauth"], env_vars: ["OPENAI_API_KEY"] }),
  provider({ id: "kimi-for-coding", name: "Kimi for Coding", env_vars: ["MOONSHOT_API_KEY"] }),
  provider({ id: "zai-coding-plan", name: "Z.AI Coding Plan", env_vars: ["ZHIPU_API_KEY"] }),
  provider({ id: "zhipuai-coding-plan", name: "Zhipu AI Coding Plan", env_vars: ["ZHIPU_API_KEY"] }),
  provider({ id: "anthropic", name: "Anthropic", env_vars: ["ANTHROPIC_API_KEY"] }),
  provider({ id: "google", name: "Google Gemini", env_vars: ["GEMINI_API_KEY"] }),
];

const catalog: CatalogProvider[] = [
  provider({ id: "anthropic", name: "Anthropic", connected: false, connection_methods: [], env_vars: ["ANTHROPIC_API_KEY"] }),
  provider({ id: "openai", name: "OpenAI", connected: false, connection_methods: [], env_vars: ["OPENAI_API_KEY"] }),
  provider({ id: "mistral", name: "Mistral", connected: false, connection_methods: [], env_vars: ["MISTRAL_API_KEY"] }),
  provider({ id: "groq", name: "Groq", connected: false, connection_methods: [], env_vars: ["GROQ_API_KEY"] }),
];

function client(
  connectedData: ConnectedProvider[],
  catalogData: CatalogProvider[],
): QueryClient {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  qc.setQueryData(userConfigKeys.connectedProviders(SELF_TARGET), connectedData);
  qc.setQueryData(userConfigKeys.catalog(SELF_TARGET), catalogData);
  return qc;
}

const meta = {
  title: "Settings/Connections",
  component: ConnectionsTab,
  parameters: { layout: "padded" },
} satisfies Meta<typeof ConnectionsTab>;

export default meta;

type Story = StoryObj<typeof meta>;

/** Codex signed in, two personal subscriptions, two org-provided API keys. */
export const Populated: Story = {
  decorators: [
    (Story) => (
      <QueryClientProvider client={client(connected, catalog)}>
        <MemoryRouter>
          <div className="mx-auto max-w-3xl p-6">
            <Story />
          </div>
        </MemoryRouter>
      </QueryClientProvider>
    ),
  ],
};

/** No connections yet — Codex sign-in CTA + empty org-provided state. */
export const Empty: Story = {
  decorators: [
    (Story) => (
      <QueryClientProvider client={client([], catalog)}>
        <MemoryRouter>
          <div className="mx-auto max-w-3xl p-6">
            <Story />
          </div>
        </MemoryRouter>
      </QueryClientProvider>
    ),
  ],
};
