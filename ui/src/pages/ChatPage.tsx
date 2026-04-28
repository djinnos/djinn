import { useEffect } from 'react';
import { useQuery } from '@tanstack/react-query';
import { ChatSessionList } from '@/components/chat/ChatSessionList';
import { ChatView } from '@/components/chat/ChatView';
import { CodeRefsPanel } from '@/components/chat/CodeRefsPanel';
import { listChatSessions } from '@/api/chatSessions';
import { useChatStore } from '@/stores/chatStore';

export function ChatPage() {
  const setSessions = useChatStore((state) => state.setSessions);
  const setActiveSession = useChatStore((state) => state.setActiveSession);

  // Sessions are server-owned — TanStack Query fetches the list, and the store
  // is seeded so the sidebar / title bar can read them synchronously.
  const { data: sessions } = useQuery({
    queryKey: ['chat-sessions'],
    queryFn: listChatSessions,
  });

  useEffect(() => {
    if (sessions) {
      setSessions(sessions);
    }
  }, [sessions, setSessions]);

  // Chat is user-scoped under the chat-user-global refactor: the sidebar
  // renders whenever the user has any sessions at all, regardless of
  // which project is selected elsewhere in the UI. New sessions are
  // created without a projectSlug — chat sessions don't pin to a project.
  const hasSessions = (sessions?.length ?? 0) > 0;

  return (
    <div className="flex min-h-0 flex-1">
      {hasSessions && (
        <ChatSessionList
          onSelectSession={(id) => setActiveSession(id)}
          onNewChat={() => {
            // Clearing the active session pops the empty state back up; a
            // fresh id is minted on the first send.
            setActiveSession(null);
          }}
        />
      )}
      <div className="flex min-h-0 flex-1">
        <ChatView />
      </div>
      {hasSessions && <CodeRefsPanel />}
    </div>
  );
}
