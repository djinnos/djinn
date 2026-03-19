import type { Meta, StoryObj } from "@storybook/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { SettingsPage } from "./SettingsPage";

/**
 * SettingsPage is store-dependent. We mock the hooks it uses so the shell
 * renders without needing a live backend. The mocks are registered in the
 * `beforeEach` callback to keep each story isolated.
 */

// --- Mock data used by the hooks ------------------------------------------

const mockProviders = [
  { id: "anthropic", name: "Anthropic", description: "Claude models", configured: true, requires_api_key: true, oauth_supported: false },
  { id: "openai", name: "OpenAI", description: "GPT models", configured: true, requires_api_key: true, oauth_supported: false },
];

const mockModels = [
  {
    model: "claude-sonnet-4-20250514",
    provider: "anthropic",
    enabledRoles: ["worker" as const, "task_reviewer" as const, "groomer" as const],
    max_concurrent: 3,
    current_active: 0,
  },
  {
    model: "gpt-4o",
    provider: "openai",
    enabledRoles: ["worker" as const, "pm" as const],
    max_concurrent: 2,
    current_active: 0,
  },
];

const mockAvailableModels = [
  {
    id: "claude-sonnet-4-20250514",
    name: "Claude Sonnet 4",
    provider_id: "anthropic",
    attachment: false,
    context_window: 200000,
    output_limit: 16384,
    pricing: { input_per_million: 3, output_per_million: 15, cache_read_per_million: 0.3, cache_write_per_million: 3.75 },
    reasoning: false,
    tool_call: true,
  },
  {
    id: "gpt-4o",
    name: "GPT-4o",
    provider_id: "openai",
    attachment: false,
    context_window: 128000,
    output_limit: 16384,
    pricing: { input_per_million: 2.5, output_per_million: 10, cache_read_per_million: 1.25, cache_write_per_million: 0 },
    reasoning: false,
    tool_call: true,
  },
];

const noop = () => {};
const asyncNoop = async () => {};

// --- vi.mock calls --------------------------------------------------------
// We mock the hooks that SettingsPage's sub-components call so they never hit
// the real backend. We also mock Tauri window APIs.

import { vi } from "vitest";

// Mock Tauri window (used for startDragging)
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ startDragging: async () => {} }),
}));

// Mock selectDirectory (used by ProjectsSettings)
vi.mock("@/tauri/commands", () => ({
  selectDirectory: async () => null,
}));

// Mock server API (used by ProjectsSettings)
vi.mock("@/api/server", () => ({
  fetchProjects: async () => [],
  addProject: async () => {},
  removeProject: async () => {},
  updateProject: async () => {},
}));

// Mock sonner toast
vi.mock("sonner", () => ({
  toast: { success: noop, error: noop },
}));

// Mock useProviders hook
vi.mock("@/hooks/settings/useProviders", () => ({
  useProviders: () => ({
    providers: mockProviders,
    configuredProviders: mockProviders,
    unconfiguredProviders: [],
    loading: false,
    loadError: null,
    validationStatus: null,
    validating: false,
    saving: false,
    oauthInProgress: false,
    setValidationStatus: noop,
    loadData: asyncNoop,
    validateInline: asyncNoop,
    saveProvider: asyncNoop,
    connectOAuth: asyncNoop,
    addCustom: asyncNoop,
    removeProvider: noop,
  }),
}));

// Mock useAgentConfig hook
vi.mock("@/hooks/settings/useAgentConfig", () => ({
  useAgentConfig: () => ({
    models: mockModels,
    availableModels: mockAvailableModels,
    isLoading: false,
    isSaving: false,
    error: null,
    hasUnsavedChanges: false,
    handleResetWizard: noop,
    onAddModel: noop,
    onRemoveModel: noop,
    onReorderModels: noop,
    onToggleRole: noop,
    onUpdateMaxSessions: noop,
    onDismissError: noop,
    onSave: noop,
  }),
}));

// Helper to wrap SettingsPage in a MemoryRouter at the right path
function SettingsStory({ initialPath }: { initialPath: string }) {
  return (
    <MemoryRouter initialEntries={[initialPath]}>
      <Routes>
        <Route path="/settings/:category" element={<SettingsPage />} />
        <Route path="/settings" element={<SettingsPage />} />
      </Routes>
    </MemoryRouter>
  );
}

const meta: Meta<typeof SettingsStory> = {
  title: "Pages/Settings",
  component: SettingsStory,
  parameters: { layout: "fullscreen" },
};

export default meta;
type Story = StoryObj<typeof SettingsStory>;

export const AgentsTab: Story = {
  args: { initialPath: "/settings/agents" },
};

export const ProvidersTab: Story = {
  args: { initialPath: "/settings/providers" },
};

export const ProjectsTab: Story = {
  args: { initialPath: "/settings/projects" },
};
