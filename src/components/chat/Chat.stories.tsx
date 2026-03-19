import type { Meta, StoryObj } from '@storybook/react-vite';
import { fn } from '@storybook/test';
import { useEffect } from 'react';
import { ChatEmptyState } from './ChatEmptyState';
import { ChatInput } from './ChatInput';
import { ChatMessageBubble } from './ChatMessageBubble';
import { ChatSessionList } from './ChatSessionList';
import { useChatStore } from '@/stores/chatStore';
import { projectStore } from '@/stores/useProjectStore';
import type { ChatMessage } from '@/stores/chatStore';

/* ------------------------------------------------------------------ */
/*  Meta                                                               */
/* ------------------------------------------------------------------ */

const meta: Meta = {
  title: 'Chat',
};

export default meta;

/* ------------------------------------------------------------------ */
/*  Shared mock data                                                   */
/* ------------------------------------------------------------------ */

const modelNameById = new Map([
  ['claude-sonnet-4-6', 'Claude Sonnet 4.6'],
  ['gpt-4o', 'GPT-4o'],
]);

const groupedModels = [
  {
    providerId: 'anthropic',
    providerLabel: 'Anthropic',
    models: [{ id: 'claude-sonnet-4-6', name: 'Claude Sonnet 4.6' }],
  },
  {
    providerId: 'openai',
    providerLabel: 'OpenAI',
    models: [{ id: 'gpt-4o', name: 'GPT-4o' }],
  },
];

/* ------------------------------------------------------------------ */
/*  1. ChatEmptyState                                                  */
/* ------------------------------------------------------------------ */

export const EmptyState_Default: StoryObj = {
  name: 'EmptyState / Default',
  render: () => (
    <div className="h-[500px] w-[700px]">
      <ChatEmptyState onPromptClick={fn()} />
    </div>
  ),
};

/* ------------------------------------------------------------------ */
/*  2. ChatInput                                                       */
/* ------------------------------------------------------------------ */

export const Input_Idle: StoryObj = {
  name: 'Input / Idle',
  render: () => (
    <div className="w-[600px]">
      <ChatInput
        onSend={fn()}
        onStop={fn()}
        onModelChange={fn()}
        streaming={false}
        selectedModel="claude-sonnet-4-6"
        modelNameById={modelNameById}
        groupedModels={groupedModels}
      />
    </div>
  ),
};

export const Input_WithText: StoryObj = {
  name: 'Input / WithText',
  render: () => (
    <div className="w-[600px]">
      <ChatInput
        onSend={fn()}
        onStop={fn()}
        onModelChange={fn()}
        streaming={false}
        selectedModel="claude-sonnet-4-6"
        modelNameById={modelNameById}
        groupedModels={groupedModels}
        prefillValue="Show me all open tasks in the Platform Foundation epic"
      />
    </div>
  ),
};

export const Input_Streaming: StoryObj = {
  name: 'Input / Streaming',
  render: () => (
    <div className="w-[600px]">
      <ChatInput
        onSend={fn()}
        onStop={fn()}
        onModelChange={fn()}
        streaming={true}
        selectedModel="claude-sonnet-4-6"
        modelNameById={modelNameById}
        groupedModels={groupedModels}
      />
    </div>
  ),
};

export const Input_NoModels: StoryObj = {
  name: 'Input / NoModels',
  render: () => (
    <div className="w-[600px]">
      <ChatInput
        onSend={fn()}
        onStop={fn()}
        onModelChange={fn()}
        streaming={false}
        selectedModel="unknown/model"
        modelNameById={new Map()}
        groupedModels={[]}
      />
    </div>
  ),
};

/* ------------------------------------------------------------------ */
/*  3. ChatMessageBubble                                               */
/* ------------------------------------------------------------------ */

const userMsg: ChatMessage = {
  id: 'msg-u1',
  role: 'user',
  content: 'What tasks are still open in the Platform Foundation epic?',
  createdAt: Date.now() - 60_000,
};

const assistantMsg: ChatMessage = {
  id: 'msg-a1',
  role: 'assistant',
  content:
    'There are **3 open tasks** in the Platform Foundation epic:\n\n' +
    '1. Ship task board keyboard navigation\n' +
    '2. Improve task metadata formatting\n' +
    '3. Refine empty-state copy\n\n' +
    'Would you like me to show more details on any of them?',
  createdAt: Date.now() - 30_000,
};

const assistantWithTools: ChatMessage = {
  id: 'msg-a2',
  role: 'assistant',
  content: 'I looked up the open tasks for you. Here is what I found.',
  toolCalls: [
    { name: 'epic_tasks', success: true },
    { name: 'task_show', success: true },
    { name: 'memory_search', success: false },
  ],
  createdAt: Date.now() - 20_000,
};

const longMsg: ChatMessage = {
  id: 'msg-a3',
  role: 'assistant',
  content:
    'Here is an example implementation for the SSE reconnection logic:\n\n' +
    '```typescript\n' +
    'class SSEClient {\n' +
    '  private eventSource: EventSource | null = null;\n' +
    '  private retryCount = 0;\n' +
    '  private maxRetries = 10;\n' +
    '\n' +
    '  connect(url: string) {\n' +
    '    this.eventSource = new EventSource(url);\n' +
    '\n' +
    '    this.eventSource.onopen = () => {\n' +
    '      this.retryCount = 0;\n' +
    "      console.log('SSE connection established');\n" +
    '    };\n' +
    '\n' +
    '    this.eventSource.onerror = () => {\n' +
    '      this.eventSource?.close();\n' +
    '      if (this.retryCount < this.maxRetries) {\n' +
    '        const delay = Math.min(1000 * 2 ** this.retryCount, 30000);\n' +
    '        this.retryCount++;\n' +
    '        setTimeout(() => this.connect(url), delay);\n' +
    '      }\n' +
    '    };\n' +
    '\n' +
    '    this.eventSource.onmessage = (event) => {\n' +
    '      const data = JSON.parse(event.data);\n' +
    '      this.handleEvent(data);\n' +
    '    };\n' +
    '  }\n' +
    '\n' +
    '  private handleEvent(data: unknown) {\n' +
    '    // Process incoming events\n' +
    '  }\n' +
    '\n' +
    '  disconnect() {\n' +
    '    this.eventSource?.close();\n' +
    '    this.eventSource = null;\n' +
    '  }\n' +
    '}\n' +
    '```\n\n' +
    'The key parts are:\n' +
    '- **Exponential backoff** with a cap at 30 seconds\n' +
    '- **Retry counter reset** on successful connection\n' +
    '- **Max retry limit** to prevent infinite loops',
  createdAt: Date.now() - 10_000,
};

export const Message_User: StoryObj = {
  name: 'Message / User',
  render: () => (
    <div className="max-w-2xl p-4">
      <ChatMessageBubble message={userMsg} />
    </div>
  ),
};

export const Message_Assistant: StoryObj = {
  name: 'Message / Assistant',
  render: () => (
    <div className="max-w-2xl p-4">
      <ChatMessageBubble message={assistantMsg} />
    </div>
  ),
};

export const Message_AssistantWithToolCalls: StoryObj = {
  name: 'Message / AssistantWithToolCalls',
  render: () => (
    <div className="max-w-2xl p-4">
      <ChatMessageBubble message={assistantWithTools} />
    </div>
  ),
};

export const Message_LongMessage: StoryObj = {
  name: 'Message / LongMessage',
  render: () => (
    <div className="max-w-2xl p-4">
      <ChatMessageBubble message={longMsg} />
    </div>
  ),
};

/* ------------------------------------------------------------------ */
/*  4. ChatSessionList                                                 */
/* ------------------------------------------------------------------ */

const mockSessions = [
  { id: 's1', title: 'Planning next milestone', updatedAt: Date.now() - 600_000 },
  { id: 's2', title: 'Debug SSE reconnection', updatedAt: Date.now() - 86_400_000 },
  { id: 's3', title: 'New Chat', updatedAt: Date.now() - 172_800_000 },
];

function SessionListSeeder({
  sessions,
  activeSessionId,
  children,
}: {
  sessions: typeof mockSessions;
  activeSessionId?: string;
  children: React.ReactNode;
}) {
  useEffect(() => {
    projectStore.setState({ selectedProjectId: '__all__' });
    useChatStore.setState({
      sessions: sessions.map((s) => ({
        id: s.id,
        title: s.title,
        projectPath: null,
        model: 'claude-sonnet-4-6',
        createdAt: s.updatedAt - 300_000,
        updatedAt: s.updatedAt,
      })),
      activeSessionId: activeSessionId ?? sessions[0]?.id ?? null,
      streamingBySession: {},
    });
    return () => {
      useChatStore.setState({ sessions: [], activeSessionId: null, streamingBySession: {} });
    };
  }, [sessions, activeSessionId]);

  return <>{children}</>;
}

export const SessionList_WithSessions: StoryObj = {
  name: 'SessionList / WithSessions',
  render: () => (
    <SessionListSeeder sessions={mockSessions} activeSessionId="s1">
      <div className="h-[500px]">
        <ChatSessionList onSelectSession={fn()} onNewChat={fn()} />
      </div>
    </SessionListSeeder>
  ),
};

export const SessionList_Empty: StoryObj = {
  name: 'SessionList / Empty',
  render: () => (
    <SessionListSeeder sessions={[]}>
      <div className="h-[500px]">
        <ChatSessionList onSelectSession={fn()} onNewChat={fn()} />
      </div>
    </SessionListSeeder>
  ),
};
