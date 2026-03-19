import type { Meta, StoryObj } from "@storybook/react";
import { AgentConfig } from "./AgentConfig";
import type { UnifiedModelEntry } from "@/stores/settingsStore";
import type { ProviderModel } from "@/api/settings";
import { fn } from "@storybook/test";

const availableModels: ProviderModel[] = [
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
    id: "claude-opus-4-20250514",
    name: "Claude Opus 4",
    provider_id: "anthropic",
    attachment: false,
    context_window: 200000,
    output_limit: 16384,
    pricing: { input_per_million: 15, output_per_million: 75, cache_read_per_million: 1.5, cache_write_per_million: 18.75 },
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

const threeModels: UnifiedModelEntry[] = [
  {
    model: "claude-sonnet-4-20250514",
    provider: "anthropic",
    enabledRoles: ["worker", "task_reviewer", "groomer"],
    max_concurrent: 3,
    current_active: 0,
  },
  {
    model: "gpt-4o",
    provider: "openai",
    enabledRoles: ["worker", "conflict_resolver"],
    max_concurrent: 2,
    current_active: 0,
  },
  {
    model: "claude-opus-4-20250514",
    provider: "anthropic",
    enabledRoles: ["pm", "task_reviewer", "conflict_resolver"],
    max_concurrent: 1,
    current_active: 0,
  },
];

const actions = {
  onAddModel: fn(),
  onRemoveModel: fn(),
  onReorderModels: fn(),
  onToggleRole: fn(),
  onUpdateMaxSessions: fn(),
  onDismissError: fn(),
  onSave: fn(),
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

export const WithModels: Story = {
  args: {
    models: threeModels,
  },
};

export const Empty: Story = {
  args: {
    models: [],
  },
};

export const Loading: Story = {
  args: {
    isLoading: true,
    models: [],
  },
};

export const WithError: Story = {
  args: {
    models: threeModels,
    error: "Failed to load settings: connection to server timed out after 30s",
  },
};

export const UnsavedChanges: Story = {
  args: {
    models: threeModels,
    hasUnsavedChanges: true,
  },
};
