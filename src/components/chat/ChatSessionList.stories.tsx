import type { Meta, StoryObj } from '@storybook/react-vite';
import { useEffect } from 'react';
import { ChatSessionList } from './ChatSessionList';
import { useChatStore } from '@/stores/chatStore';
import { projectStore } from '@/stores/useProjectStore';

const now = Date.now();
const hoursAgo = (h: number) => now - h * 3_600_000;

/**
 * Wrapper that seeds the chat and project stores before rendering.
 */
function ChatSessionListSeeded({
  onSelectSession,
  onNewChat,
  sessions,
  streamingSessionId,
  activeSessionId,
}: {
  onSelectSession: (id: string) => void;
  onNewChat: () => void;
  sessions: Array<{ id: string; title: string; updatedAt: number }>;
  streamingSessionId?: string;
  activeSessionId?: string;
}) {
  useEffect(() => {
    // Set project store to "all projects" so sessions with null projectPath are visible
    projectStore.setState({ selectedProjectId: '__all__' });

    // Seed chat store
    useChatStore.setState({
      sessions: sessions.map((s) => ({
        id: s.id,
        title: s.title,
        projectPath: null,
        model: 'claude-sonnet-4-6',
        createdAt: s.updatedAt - 300_000,
        updatedAt: s.updatedAt,
      })),
      activeSessionId: activeSessionId ?? null,
      streamingBySession: streamingSessionId
        ? { [streamingSessionId]: 'Thinking...' }
        : {},
    });

    return () => {
      useChatStore.setState({
        sessions: [],
        activeSessionId: null,
        streamingBySession: {},
      });
    };
  }, [sessions, streamingSessionId, activeSessionId]);

  return <ChatSessionList onSelectSession={onSelectSession} onNewChat={onNewChat} />;
}

const meta: Meta<typeof ChatSessionListSeeded> = {
  title: 'Chat/ChatSessionList',
  component: ChatSessionListSeeded,
  args: {
    onSelectSession: () => {},
    onNewChat: () => {},
  },
  decorators: [
    (Story) => (
      <div className="h-[600px]">
        <Story />
      </div>
    ),
  ],
};

export default meta;
type Story = StoryObj<typeof ChatSessionListSeeded>;

const sampleSessions = [
  { id: 'sess-1', title: 'Fix SSE reconnection bug', updatedAt: hoursAgo(0.5) },
  { id: 'sess-2', title: 'Plan Q2 milestone roadmap', updatedAt: hoursAgo(2) },
  { id: 'sess-3', title: 'Review authentication epic', updatedAt: hoursAgo(5) },
  { id: 'sess-4', title: 'Refactor Zustand stores', updatedAt: hoursAgo(26) },
  { id: 'sess-5', title: 'Create onboarding flow tasks', updatedAt: hoursAgo(28) },
  { id: 'sess-6', title: 'Debug Tauri sidecar startup', updatedAt: hoursAgo(50) },
  { id: 'sess-7', title: 'Design settings page layout', updatedAt: hoursAgo(72) },
];

export const WithSessions: Story = {
  args: {
    sessions: sampleSessions,
    activeSessionId: 'sess-1',
  },
};

export const Empty: Story = {
  args: {
    sessions: [],
  },
};

export const WithStreaming: Story = {
  args: {
    sessions: sampleSessions,
    activeSessionId: 'sess-2',
    streamingSessionId: 'sess-2',
  },
};
