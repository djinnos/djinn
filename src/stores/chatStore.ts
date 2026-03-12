import { create } from 'zustand';
import { persist } from 'zustand/middleware';

const STORAGE_KEY = 'djinnos-chat-sessions';
const AUTO_TITLE_MAX_LENGTH = 50;

export interface ChatSession {
  id: string;
  title: string;
  projectPath: string | null;
  createdAt: number;
  updatedAt: number;
}

export interface ChatMessage {
  id: string;
  role: 'user' | 'assistant';
  content: string;
  toolCalls?: { name: string; success?: boolean }[];
  createdAt: number;
}

export interface ChatState {
  sessions: ChatSession[];
  messagesBySession: Record<string, ChatMessage[]>;
  streamingBySession: Record<string, string>;
  loadingBySession: Record<string, boolean>;
  activeSessionId: string | null;

  createSession: (projectPath?: string | null) => string;
  deleteSession: (sessionId: string) => void;
  setActiveSession: (sessionId: string | null) => void;
  addMessage: (sessionId: string, message: ChatMessage) => void;
  appendStreamingText: (sessionId: string, chunk: string) => void;
  finalizeStreaming: (sessionId: string, message?: Omit<ChatMessage, 'content'> & { content?: string }) => void;
  clearStreaming: (sessionId: string) => void;
  getSessionsForProject: (projectPath: string | null) => ChatSession[];
}

function generateId(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID();
  }

  return `${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
}

function toAutoTitle(content: string): string {
  const trimmed = content.trim().replace(/\s+/g, ' ');
  if (!trimmed) return 'New Chat';
  if (trimmed.length <= AUTO_TITLE_MAX_LENGTH) return trimmed;
  return `${trimmed.slice(0, AUTO_TITLE_MAX_LENGTH - 1)}…`;
}

export const useChatStore = create<ChatState>()(
  persist(
    (set, get) => ({
      sessions: [],
      messagesBySession: {},
      streamingBySession: {},
      loadingBySession: {},
      activeSessionId: null,

      createSession: (projectPath = null) => {
        const now = Date.now();
        const id = generateId();
        const newSession: ChatSession = {
          id,
          title: 'New Chat',
          projectPath,
          createdAt: now,
          updatedAt: now,
        };

        set((state) => ({
          sessions: [newSession, ...state.sessions],
          activeSessionId: id,
          messagesBySession: { ...state.messagesBySession, [id]: [] },
          streamingBySession: { ...state.streamingBySession, [id]: '' },
          loadingBySession: { ...state.loadingBySession, [id]: false },
        }));

        return id;
      },

      deleteSession: (sessionId) => {
        set((state) => {
          const { [sessionId]: _messages, ...messagesBySession } = state.messagesBySession;
          const { [sessionId]: _streaming, ...streamingBySession } = state.streamingBySession;
          const { [sessionId]: _loading, ...loadingBySession } = state.loadingBySession;

          return {
            sessions: state.sessions.filter((session) => session.id !== sessionId),
            messagesBySession,
            streamingBySession,
            loadingBySession,
            activeSessionId:
              state.activeSessionId === sessionId
                ? (state.sessions.find((session) => session.id !== sessionId)?.id ?? null)
                : state.activeSessionId,
          };
        });
      },

      setActiveSession: (sessionId) => {
        set({ activeSessionId: sessionId });
      },

      addMessage: (sessionId, message) => {
        set((state) => {
          const existingMessages = state.messagesBySession[sessionId] ?? [];
          const nextMessages = [...existingMessages, message];
          const existingSession = state.sessions.find((session) => session.id === sessionId);

          if (!existingSession) {
            return {
              messagesBySession: {
                ...state.messagesBySession,
                [sessionId]: nextMessages,
              },
            };
          }

          const shouldAutoTitle =
            existingSession.title === 'New Chat' &&
            message.role === 'user' &&
            existingMessages.filter((m) => m.role === 'user').length === 0;

          return {
            messagesBySession: {
              ...state.messagesBySession,
              [sessionId]: nextMessages,
            },
            sessions: state.sessions.map((session) =>
              session.id === sessionId
                ? {
                    ...session,
                    title: shouldAutoTitle ? toAutoTitle(message.content) : session.title,
                    updatedAt: Date.now(),
                  }
                : session
            ),
          };
        });
      },

      appendStreamingText: (sessionId, chunk) => {
        set((state) => ({
          streamingBySession: {
            ...state.streamingBySession,
            [sessionId]: `${state.streamingBySession[sessionId] ?? ''}${chunk}`,
          },
          loadingBySession: {
            ...state.loadingBySession,
            [sessionId]: true,
          },
        }));
      },

      finalizeStreaming: (sessionId, message) => {
        set((state) => {
          const content = message?.content ?? state.streamingBySession[sessionId] ?? '';
          const shouldAddMessage = content.trim().length > 0;
          const nextMessages = shouldAddMessage
            ? [
                ...(state.messagesBySession[sessionId] ?? []),
                {
                  id: message?.id ?? generateId(),
                  role: message?.role ?? 'assistant',
                  content,
                  toolCalls: message?.toolCalls,
                  createdAt: message?.createdAt ?? Date.now(),
                },
              ]
            : (state.messagesBySession[sessionId] ?? []);

          return {
            messagesBySession: {
              ...state.messagesBySession,
              [sessionId]: nextMessages,
            },
            streamingBySession: {
              ...state.streamingBySession,
              [sessionId]: '',
            },
            loadingBySession: {
              ...state.loadingBySession,
              [sessionId]: false,
            },
            sessions: state.sessions.map((session) =>
              session.id === sessionId ? { ...session, updatedAt: Date.now() } : session
            ),
          };
        });
      },

      clearStreaming: (sessionId) => {
        set((state) => ({
          streamingBySession: {
            ...state.streamingBySession,
            [sessionId]: '',
          },
          loadingBySession: {
            ...state.loadingBySession,
            [sessionId]: false,
          },
        }));
      },

      getSessionsForProject: (projectPath) => {
        return get().sessions.filter((session) => session.projectPath === projectPath);
      },
    }),
    {
      name: STORAGE_KEY,
      partialize: (state) => ({
        sessions: state.sessions,
        activeSessionId: state.activeSessionId,
      }),
    }
  )
);
