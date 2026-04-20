import type { Meta, StoryObj } from '@storybook/react-vite';
import { ChatMessageBubble } from './ChatMessageBubble';
import type { ChatMessage } from '@/stores/chatStore';

const now = Date.now();

const meta: Meta<typeof ChatMessageBubble> = {
  title: 'Chat/ChatMessageBubble',
  component: ChatMessageBubble,
  decorators: [
    (Story) => (
      <div className="w-[600px] space-y-4 p-4">
        <Story />
      </div>
    ),
  ],
};

export default meta;
type Story = StoryObj<typeof ChatMessageBubble>;

export const UserMessage: Story = {
  args: {
    message: {
      id: 'msg-1',
      role: 'user',
      content: 'Can you show me the status of my epics and highlight any that are behind schedule?',
      createdAt: now - 60_000,
    } satisfies ChatMessage,
  },
};

export const AssistantMessage: Story = {
  args: {
    message: {
      id: 'msg-2',
      role: 'assistant',
      content:
        "Here's a summary of your epics:\n\n" +
        '| Epic | Status | Progress |\n' +
        '|------|--------|----------|\n' +
        '| Platform Foundation | Active | 8/12 tasks done |\n' +
        '| Authentication | Active | 3/6 tasks done |\n' +
        '| Onboarding | Not started | 0/4 tasks done |\n\n' +
        'The **Authentication** epic looks a bit behind -- it was expected to be further along by now. ' +
        'Would you like me to break down the remaining tasks?',
      createdAt: now - 30_000,
    } satisfies ChatMessage,
  },
};

export const AssistantWithToolCalls: Story = {
  args: {
    message: {
      id: 'msg-3',
      role: 'assistant',
      content:
        'I found **3 open tasks** assigned to you. The highest priority one is *"Fix SSE reconnection on server restart"* (priority P0).\n\n' +
        'Want me to show the details or update any of these?',
      toolCalls: [
        { name: 'task_list', success: true },
        { name: 'epic_tasks', success: true },
        { name: 'project_config_get', success: true },
      ],
      createdAt: now - 15_000,
    } satisfies ChatMessage,
  },
};

export const LongCodeBlock: Story = {
  args: {
    message: {
      id: 'msg-4',
      role: 'assistant',
      content:
        "Here's an example of how to create a Zustand store with persistence:\n\n" +
        '```typescript\n' +
        "import { create } from 'zustand';\n" +
        "import { persist } from 'zustand/middleware';\n" +
        '\n' +
        'interface CounterState {\n' +
        '  count: number;\n' +
        '  increment: () => void;\n' +
        '  decrement: () => void;\n' +
        '  reset: () => void;\n' +
        '}\n' +
        '\n' +
        'export const useCounterStore = create<CounterState>()(\n' +
        '  persist(\n' +
        '    (set) => ({\n' +
        '      count: 0,\n' +
        '      increment: () => set((state) => ({ count: state.count + 1 })),\n' +
        '      decrement: () => set((state) => ({ count: state.count - 1 })),\n' +
        '      reset: () => set({ count: 0 }),\n' +
        '    }),\n' +
        "    { name: 'counter-storage' }\n" +
        '  )\n' +
        ');\n' +
        '```\n\n' +
        'The `persist` middleware will automatically save and restore the store state from `localStorage`.',
      createdAt: now - 5_000,
    } satisfies ChatMessage,
  },
};
