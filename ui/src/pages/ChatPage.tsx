import { ChatSessionList } from '@/components/chat/ChatSessionList';
import { ChatView } from '@/components/chat/ChatView';
import { useChatStore } from '@/stores/chatStore';

export function ChatPage() {
  const createSession = useChatStore((state) => state.createSession);
  const setActiveSession = useChatStore((state) => state.setActiveSession);
  const sessions = useChatStore((state) => state.sessions);

  // Chat is user-scoped under the chat-user-global refactor: the sidebar
  // renders whenever the user has any sessions at all, regardless of
  // which project is selected elsewhere in the UI. New sessions are
  // created without a projectSlug — chat sessions don't pin to a project.
  const hasSessions = sessions.length > 0;

  return (
    <div className="flex min-h-0 flex-1">
      {hasSessions && (
        <ChatSessionList
          onSelectSession={(id) => setActiveSession(id)}
          onNewChat={() => {
            const sessionId = createSession();
            setActiveSession(sessionId);
          }}
        />
      )}
      <div className="flex min-h-0 flex-1">
        <ChatView />
      </div>
    </div>
  );
}
