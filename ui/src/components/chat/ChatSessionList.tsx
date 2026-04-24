import { useEffect, useMemo, useState } from 'react';
import { Edit02Icon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import { Input } from '@/components/ui/input';
import { useChatStore } from '@/stores/chatStore';
import { cn } from '@/lib/utils';

interface ChatSessionListProps {
  onSelectSession: (id: string) => void;
  onNewChat: () => void;
}

function getDateGroupLabel(timestamp: number): string {
  const date = new Date(timestamp);
  const now = new Date();

  const startOfDate = new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
  const startOfToday = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
  const dayDiff = Math.floor((startOfToday - startOfDate) / 86_400_000);

  if (dayDiff === 0) return 'Today';
  if (dayDiff === 1) return 'Yesterday';
  return date.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
}

export function ChatSessionList({ onSelectSession, onNewChat }: ChatSessionListProps) {
  const activeSessionId = useChatStore((state) => state.activeSessionId);
  const sessions = useChatStore((state) => state.sessions);
  const streamingBySession = useChatStore((state) => state.streamingBySession);

  const [search, setSearch] = useState('');
  const [debouncedSearch, setDebouncedSearch] = useState('');

  useEffect(() => {
    const timer = window.setTimeout(() => {
      setDebouncedSearch(search.trim().toLowerCase());
    }, 200);

    return () => window.clearTimeout(timer);
  }, [search]);

  // Chat is user-scoped under the chat-user-global refactor: sessions are
  // not filtered by the currently-selected project. Every session the user
  // has started shows in the sidebar.
  const filteredSessions = useMemo(() => {
    return sessions
      .filter((session) => session.title.toLowerCase().includes(debouncedSearch))
      .sort((a, b) => b.updatedAt - a.updatedAt);
  }, [sessions, debouncedSearch]);

  const groupedSessions = useMemo(() => {
    const groups: Array<{ label: string; sessions: typeof filteredSessions }> = [];
    for (const session of filteredSessions) {
      const label = getDateGroupLabel(session.updatedAt);
      const existing = groups.find((group) => group.label === label);
      if (existing) {
        existing.sessions.push(session);
      } else {
        groups.push({ label, sessions: [session] });
      }
    }
    return groups;
  }, [filteredSessions]);

  return (
    <aside className="w-72 border-r border-border p-3">
      <Input
        value={search}
        onChange={(event) => setSearch(event.target.value)}
        placeholder="Search chats"
        className="mb-3"
        aria-label="Search chats"
      />

      <button
        type="button"
        onClick={onNewChat}
        className="mb-2 flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm text-muted-foreground hover:text-foreground hover:bg-muted transition-colors"
      >
        <HugeiconsIcon icon={Edit02Icon} size={14} />
        New chat
      </button>

      {groupedSessions.length === 0 ? (
        <p className="rounded-md border border-dashed border-border p-3 text-sm text-muted-foreground">
          No chats found.
        </p>
      ) : (
        <div className="space-y-3">
          {groupedSessions.map((group) => (
            <div key={group.label}>
              <p className="mb-1 px-2 text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
                {group.label}
              </p>
              <div className="space-y-0.5">
                {group.sessions.map((session) => {
                  const isStreaming = Boolean(streamingBySession[session.id]);
                  return (
                    <button
                      key={session.id}
                      type="button"
                      onClick={() => onSelectSession(session.id)}
                      className={cn(
                        'w-full rounded-md px-2 py-1.5 text-left hover:bg-muted transition-colors',
                        activeSessionId === session.id && 'bg-muted'
                      )}
                    >
                      <div className="flex items-center gap-2">
                        <p className="min-w-0 flex-1 truncate text-sm">{session.title}</p>
                        {isStreaming && (
                          <span className="inline-block h-1.5 w-1.5 shrink-0 rounded-full bg-primary animate-pulse" aria-label="Streaming" />
                        )}
                      </div>
                    </button>
                  );
                })}
              </div>
            </div>
          ))}
        </div>
      )}
    </aside>
  );
}
