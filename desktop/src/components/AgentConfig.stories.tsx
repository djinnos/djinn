import type { Meta, StoryObj } from "@storybook/react-vite";
import { AgentConfig } from "./AgentConfig";
import type { ModelEntry } from "@/stores/settingsStore";
import type { ProviderModel } from "@/api/settings";

const availableModels: ProviderModel[] = [
  {
    id: "claude-sonnet-4-6",
    name: "Claude Sonnet 4.6",
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
  {
    id: "deepseek-coder",
    name: "DeepSeek Coder",
    provider_id: "deepseek",
    attachment: false,
    context_window: 128000,
    output_limit: 8192,
    pricing: { input_per_million: 0.14, output_per_million: 0.28, cache_read_per_million: 0, cache_write_per_million: 0 },
    reasoning: false,
    tool_call: true,
  },
  {
    id: "gemini-2.5-pro",
    name: "Gemini 2.5 Pro",
    provider_id: "google",
    attachment: false,
    context_window: 1000000,
    output_limit: 65536,
    pricing: { input_per_million: 1.25, output_per_million: 10, cache_read_per_million: 0, cache_write_per_million: 0 },
    reasoning: true,
    tool_call: true,
  },
];

const threeModels: ModelEntry[] = [
  { model: "claude-sonnet-4-6", provider: "anthropic", max_concurrent: 3 },
  { model: "gpt-4o", provider: "openai", max_concurrent: 2 },
  { model: "deepseek-coder", provider: "deepseek", max_concurrent: 1 },
];

const actions = {
  onAddModel: () => {},
  onRemoveModel: () => {},
  onReorderModels: () => {},
  onToggleRole: () => {},
  onUpdateMaxSessions: () => {},
  memoryModel: null,
  onSetMemoryModel: () => {},
  onDismissError: () => {},
  onSave: () => {},
};

const meta: Meta<typeof AgentConfig> = {
  title: "Settings/AgentConfig",
  component: AgentConfig,
  parameters: { layout: "padded" },
  args: {
    ...actions,
    availableModels,
    isLoading: false,
    isSaving: false,
    error: null,
    hasUnsavedChanges: false,
    models: [],
  },
};

export default meta;
type Story = StoryObj<typeof AgentConfig>;

export const Loading: Story = {
  args: {
    isLoading: true,
    models: [],
  },
};

export const Empty: Story = {
  args: {
    models: [],
  },
};

export const WithModels: Story = {
  args: {
    models: threeModels,
  },
};

export const WithError: Story = {
  args: {
    models: threeModels,
    error: "Failed to save configuration",
    hasUnsavedChanges: true,
  },
};

export const Saving: Story = {
  args: {
    models: threeModels,
    isSaving: true,
    hasUnsavedChanges: true,
  },
};
