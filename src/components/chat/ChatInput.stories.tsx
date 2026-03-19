import type { Meta, StoryObj } from '@storybook/react-vite';
import { ChatInput } from './ChatInput';

const modelNameById = new Map([
  ['claude-sonnet-4-6', 'Claude Sonnet 4.6'],
  ['claude-opus-4', 'Claude Opus 4'],
  ['gpt-4o', 'GPT-4o'],
  ['gpt-4o-mini', 'GPT-4o mini'],
]);

const groupedModels = [
  {
    providerId: 'anthropic',
    providerLabel: 'Anthropic',
    models: [
      { id: 'claude-sonnet-4-6', name: 'Claude Sonnet 4.6' },
      { id: 'claude-opus-4', name: 'Claude Opus 4' },
    ],
  },
  {
    providerId: 'openai',
    providerLabel: 'OpenAI',
    models: [
      { id: 'gpt-4o', name: 'GPT-4o' },
      { id: 'gpt-4o-mini', name: 'GPT-4o mini' },
    ],
  },
];

const meta: Meta<typeof ChatInput> = {
  title: 'Chat/ChatInput',
  component: ChatInput,
  args: {
    onSend: () => {},
    onStop: () => {},
    onModelChange: () => {},
    streaming: false,
    selectedModel: 'claude-sonnet-4-6',
    modelNameById,
    groupedModels,
  },
  decorators: [
    (Story) => (
      <div className="w-[600px]">
        <Story />
      </div>
    ),
  ],
};

export default meta;
type Story = StoryObj<typeof ChatInput>;

export const Default: Story = {};

export const WithPrefill: Story = {
  args: {
    prefillValue: 'Show me my epics and their current status',
  },
};

export const Streaming: Story = {
  args: {
    streaming: true,
  },
};

export const NoModels: Story = {
  args: {
    selectedModel: 'unknown/model',
    modelNameById: new Map(),
    groupedModels: [],
  },
};
