import { useMemo } from 'react';
import { ChatSessionList } from '@/components/chat/ChatSessionList';
import { ChatView } from '@/components/chat/ChatView';
import { useChatStore } from '@/stores/chatStore';
import { useIsAllProjects, useSelectedProject } from '@/stores/useProjectStore';

export function ChatPage() {
  const createSession = useChatStore((state) => state.createSession);
  const setActiveSession = useChatStore((state) => state.setActiveSession);
  const sessions = useChatStore((state) => state.sessions);
  const selectedProject = useSelectedProject();
  const isAllProjects = useIsAllProjects();

  const hasSessionsInScope = useMemo(() => {
    const projectPath = isAllProjects ? null : (selectedProject?.path ?? null);
    return sessions.some((session) => session.projectPath === projectPath);
  }, [sessions, selectedProject, isAllProjects]);

  return (
    <div className="flex min-h-0 flex-1">
      {hasSessionsInScope && (
        <ChatSessionList
          onSelectSession={(id) => setActiveSession(id)}
          onNewChat={() => {
            const sessionId = createSession(isAllProjects ? null : (selectedProject?.path ?? null));
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
