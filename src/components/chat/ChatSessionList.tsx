import { useMemo } from 'react';
import { Button } from '@/components/ui/button';
import { useChatStore } from '@/stores/chatStore';
import { useSelectedProject, useIsAllProjects } from '@/stores/useProjectStore';
import { cn } from '@/lib/utils';

interface ChatSessionListProps {
  onSelectSession: (id: string) => void;
  onNewChat: () => void;
}

function relativeTime(timestamp: number): string {
  const diff = Date.now() - timestamp;
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

export function ChatSessionList({ onSelectSession, onNewChat }: ChatSessionListProps) {
  const selectedProject = useSelectedProject();
  const isAllProjects = useIsAllProjects();
  const activeSessionId = useChatStore((state) => state.activeSessionId);
  const sessions = useChatStore((state) => state.sessions);

  const filteredSessions = useMemo(() => {
    const projectPath = isAllProjects ? null : (selectedProject?.path ?? null);
    return sessions
      .filter((session) => session.projectPath === projectPath)
      .sort((a, b) => b.updatedAt - a.updatedAt);
  }, [sessions, selectedProject, isAllProjects]);

  return (
    <aside className="w-72 border-r border-border p-3">
      <Button className="mb-3 w-full" onClick={onNewChat}>New chat</Button>
      <div className="space-y-2">
        {filteredSessions.map((session) => (
          <button
            key={session.id}
            type="button"
            onClick={() => onSelectSession(session.id)}
            className={cn(
              'w-full rounded-md border border-border p-3 text-left hover:bg-muted',
              activeSessionId === session.id && 'bg-muted'
            )}
          >
            <p className="truncate text-sm font-medium">{session.title}</p>
            <p className="mt-1 text-xs text-muted-foreground">{relativeTime(session.updatedAt)}</p>
          </button>
        ))}
      </div>
    </aside>
  );
}
