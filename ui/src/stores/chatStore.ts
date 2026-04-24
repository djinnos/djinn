import { create } from 'zustand';
import { createJSONStorage, persist, type PersistOptions } from 'zustand/middleware';

/**
 * In-memory chat state.
 *
 * The server is the source of truth for sessions and messages — this store is
 * a transient UI cache + streaming buffer, seeded from TanStack Query on load.
 * Only the `activeSessionId` and per-session drafts are persisted (to
 * sessionStorage) because they are purely UI state and shouldn't survive a
 * browser restart.
 */

const UI_STORAGE_KEY = 'djinnos-chat-ui';

export interface ChatSession {
  id: string;
  title: string;
  projectSlug: string | null;
  model: string | null;
  createdAt: number;
  updatedAt: number;
}

export interface ChatAttachment {
  id: string;
  filename: string;
  mediaType: string;
  /** base64-encoded file data */
  data: string;
  /** data: URL for preview (images) */
  url?: string;
}

export interface ChatMessage {
  id: string;
  role: 'user' | 'assistant';
  content: string;
  attachments?: ChatAttachment[];
  toolCalls?: { name: string; success?: boolean; input?: unknown }[];
  createdAt: number;
}

export interface ChatState {
  /** In-memory cache of sessions, seeded from the server list query. */
  sessions: ChatSession[];
  /** In-memory cache of messages, seeded from the server messages query. */
  messagesBySession: Record<string, ChatMessage[]>;
  streamingBySession: Record<string, string>;
  loadingBySession: Record<string, boolean>;
  thinkingStartTimeBySession: Record<string, number | null>;
  /** Persisted UI state. */
  draftBySession: Record<string, string>;
  globalDraft: string;
  activeSessionId: string | null;

  /**
   * Replace the cached session list (used by `useQuery` onSuccess). Preserves
   * any locally-mutated fields that only the store knows about (currently
   * none — we keep it verbatim to stay honest about the server being canonical).
   */
  setSessions: (sessions: ChatSession[]) => void;
  /** Insert or upsert a single session (used when a new chat is started locally). */
  upsertSession: (session: ChatSession) => void;
  /** Drop a session from the cache — server delete is done via mutation. */
  removeSession: (sessionId: string) => void;
  /** Seed the messages cache for one session (from `getChatSessionMessages`). */
  setSessionMessages: (sessionId: string, messages: ChatMessage[]) => void;
  setActiveSession: (sessionId: string | null) => void;
  setSessionModel: (sessionId: string, model: string) => void;
  addMessage: (sessionId: string, message: ChatMessage) => void;
  appendStreamingText: (sessionId: string, chunk: string) => void;
  finalizeStreaming: (sessionId: string, message?: Omit<ChatMessage, 'content'> & { content?: string }) => void;
  updateSessionTitle: (sessionId: string, title: string) => void;
  clearStreaming: (sessionId: string) => void;
  setThinkingStartTime: (sessionId: string, startTime: number | null) => void;
  setDraft: (sessionId: string | null, text: string) => void;
}

interface PersistedChatState {
  draftBySession: Record<string, string>;
  globalDraft: string;
  activeSessionId: string | null;
}

const persistOptions: PersistOptions<ChatState, PersistedChatState> = {
  name: UI_STORAGE_KEY,
  // Only the transient UI state (drafts + last active session) survives
  // a reload. Sessions and messages come from the server.
  storage: createJSONStorage(() => sessionStorage),
  partialize: (state) => ({
    draftBySession: state.draftBySession,
    globalDraft: state.globalDraft,
    activeSessionId: state.activeSessionId,
  }),
};

export const useChatStore = create<ChatState>()(
  persist(
    (set) => ({
      sessions: [],
      messagesBySession: {},
      streamingBySession: {},
      loadingBySession: {},
      thinkingStartTimeBySession: {},
      draftBySession: {},
      globalDraft: '',
      activeSessionId: null,

      setSessions: (sessions) => {
        set({ sessions });
      },

      upsertSession: (session) => {
        set((state) => {
          const existingIndex = state.sessions.findIndex((s) => s.id === session.id);
          if (existingIndex === -1) {
            return { sessions: [session, ...state.sessions] };
          }
          const next = state.sessions.slice();
          next[existingIndex] = session;
          return { sessions: next };
        });
      },

      removeSession: (sessionId) => {
        set((state) => {
          const messagesBySession = { ...state.messagesBySession };
          const streamingBySession = { ...state.streamingBySession };
          const loadingBySession = { ...state.loadingBySession };
          const thinkingStartTimeBySession = { ...state.thinkingStartTimeBySession };
          delete messagesBySession[sessionId];
          delete streamingBySession[sessionId];
          delete loadingBySession[sessionId];
          delete thinkingStartTimeBySession[sessionId];

          return {
            sessions: state.sessions.filter((session) => session.id !== sessionId),
            messagesBySession,
            streamingBySession,
            loadingBySession,
            thinkingStartTimeBySession,
            activeSessionId:
              state.activeSessionId === sessionId
                ? (state.sessions.find((session) => session.id !== sessionId)?.id ?? null)
                : state.activeSessionId,
          };
        });
      },

      setSessionMessages: (sessionId, messages) => {
        set((state) => ({
          messagesBySession: {
            ...state.messagesBySession,
            [sessionId]: messages,
          },
        }));
      },

      setActiveSession: (sessionId) => {
        set({ activeSessionId: sessionId });
      },

      setSessionModel: (sessionId, model) => {
        set((state) => ({
          sessions: state.sessions.map((session) =>
            session.id === sessionId ? { ...session, model, updatedAt: Date.now() } : session
          ),
        }));
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

          return {
            messagesBySession: {
              ...state.messagesBySession,
              [sessionId]: nextMessages,
            },
            sessions: state.sessions.map((session) =>
              session.id === sessionId ? { ...session, updatedAt: Date.now() } : session
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
          thinkingStartTimeBySession: {
            ...state.thinkingStartTimeBySession,
            [sessionId]: null,
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
            thinkingStartTimeBySession: {
              ...state.thinkingStartTimeBySession,
              [sessionId]: null,
            },
            sessions: state.sessions.map((session) =>
              session.id === sessionId ? { ...session, updatedAt: Date.now() } : session
            ),
          };
        });
      },

      updateSessionTitle: (sessionId, title) => {
        const normalizedTitle = title.trim().replace(/\s+/g, ' ');
        if (!normalizedTitle) return;

        set((state) => ({
          sessions: state.sessions.map((session) =>
            session.id === sessionId
              ? { ...session, title: normalizedTitle, updatedAt: Date.now() }
              : session
          ),
        }));
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
          thinkingStartTimeBySession: {
            ...state.thinkingStartTimeBySession,
            [sessionId]: null,
          },
        }));
      },

      setThinkingStartTime: (sessionId, startTime) => {
        set((state) => ({
          thinkingStartTimeBySession: {
            ...state.thinkingStartTimeBySession,
            [sessionId]: startTime,
          },
          loadingBySession: {
            ...state.loadingBySession,
            [sessionId]: startTime !== null ? true : state.loadingBySession[sessionId] ?? false,
          },
        }));
      },

      setDraft: (sessionId, text) => {
        if (sessionId === null) {
          set({ globalDraft: text });
        } else {
          set((state) => ({
            draftBySession: {
              ...state.draftBySession,
              [sessionId]: text,
            },
          }));
        }
      },
    }),
    persistOptions
  )
);

function generateId(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID();
  }

  return `${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
}
