import { useEffect } from "react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { KanbanBoard } from "@/components/KanbanBoard";
import { ChatPage } from "@/pages/ChatPage";
import { useChatStore } from "@/stores/chatStore";
import { projectStore } from "@/stores/useProjectStore";
import type { Epic, Task } from "@/api/types";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, staleTime: Infinity } },
});

// Seed provider-models-connected so ChatView doesn't fire a real fetch
queryClient.setQueryData(["provider-models-connected"], [
  { id: "anthropic/claude-sonnet-4-6", name: "Claude Sonnet 4.6", provider_id: "anthropic" },
  { id: "openai/gpt-4o", name: "GPT-4o", provider_id: "openai" },
]);

// ---------------------------------------------------------------------------
// Kanban fixtures (mirrors KanbanBoard.stories.tsx)
// ---------------------------------------------------------------------------

const makeEpic = (id: string, title: string, emoji: string, owner: string): Epic => ({
  id,
  short_id: id.slice(0, 4),
  title,
  description: "",
  emoji,
  color: "#3B82F6",
  status: "active",
  owner,
  created_at: "2026-03-01T10:00:00.000Z",
  updated_at: "2026-03-01T10:00:00.000Z",
});

const epicsFixture: Epic[] = [
  makeEpic("epic-foundation", "Platform Foundation", "🚀", "Alex"),
  makeEpic("epic-ux", "UX Polish", "🎨", "Mina"),
  makeEpic("epic-auth", "Authentication", "🔐", "Priya"),
];

const makeTask = (
  id: string,
  title: string,
  status: string,
  priority: number,
  owner: string,
  epicId: string | undefined,
  labels: string[],
  ts: string,
  overrides?: Partial<Task>,
): Task => ({
  id,
  short_id: id.slice(0, 4),
  title,
  description: "",
  design: "",
  acceptance_criteria: [],
  issue_type: "task",
  status,
  priority,
  owner,
  epic_id: epicId,
  labels,
  memory_refs: [],
  created_at: ts,
  updated_at: ts,
  reopen_count: 0,
  continuation_count: 0,
  unresolved_blocker_count: 0,
  ...overrides,
});

const tasksFixture: Task[] = [
  makeTask("t-10", "Plugin hot-reload support", "grooming", 2, "Alex", "epic-foundation", [], "2026-03-01T10:00:00.000Z"),
  makeTask("t-11", "Session persistence layer", "grooming", 1, "Priya", "epic-auth", [], "2026-03-01T10:05:00.000Z"),
  makeTask("t-12", "System tray menu config", "ready", 1, "Alex", "epic-foundation", [], "2026-03-01T10:10:00.000Z"),
  makeTask("t-1", "Scaffold auth hooks", "open", 1, "Alex", "epic-foundation", ["auth"], "2026-03-01T11:00:00.000Z"),
  makeTask("t-13", "Setup config loader", "open", 1, "Alex", "epic-foundation", [], "2026-03-01T11:05:00.000Z"),
  makeTask("t-14", "Token refresh logic", "open", 0, "Priya", "epic-auth", [], "2026-03-01T11:10:00.000Z", { reopen_count: 2 }),
  makeTask("t-15", "Keyboard shortcut manager", "open", 1, "Mina", "epic-ux", [], "2026-03-01T11:15:00.000Z", { unresolved_blocker_count: 2 }),
  makeTask("t-5", "Backfill migration docs", "open", 3, "Jordan", undefined, ["docs"], "2026-03-01T11:40:00.000Z"),
  makeTask("t-2", "Set up observability alerts", "in_progress", 0, "Priya", "epic-foundation", ["infra"], "2026-03-01T11:10:00.000Z", {
    active_session: { model_id: "claude-3.5-sonnet", started_at: new Date(Date.now() - 720_000).toISOString() },
    duration_seconds: 300,
  }),
  makeTask("t-6", "Run integration test suite", "verifying", 1, "Priya", "epic-foundation", ["ci"], "2026-03-01T11:15:00.000Z", {
    duration_seconds: 480,
  }),
  makeTask("t-3", "Refine empty states", "needs_task_review", 2, "Mina", "epic-ux", ["ui"], "2026-03-01T11:20:00.000Z", {
    duration_seconds: 1860,
  }),
  makeTask("t-17", "Credential validation flow", "in_lead_intervention", 0, "Priya", "epic-auth", [], "2026-03-01T11:30:00.000Z", {
    duration_seconds: 360,
  }),
  makeTask("t-4", "Keyboard navigation pass", "closed", 1, "Alex", "epic-ux", ["accessibility"], "2026-03-01T11:30:00.000Z", {
    duration_seconds: 1380,
  }),
  makeTask("t-7", "SSE initial connect", "closed", 1, "Priya", "epic-foundation", [], "2026-03-01T11:35:00.000Z", {
    duration_seconds: 300,
  }),
];

// ---------------------------------------------------------------------------
// Chat fixtures
// ---------------------------------------------------------------------------

const MOCK_PROJECT_SLUG = "djinnos/djinnos";
const MOCK_PROJECT = {
  id: "proj-1",
  name: "Djinn OS",
  github_owner: "djinnos",
  github_repo: "djinnos",
};

const mockSessions = [
  {
    id: "s1",
    title: "Planning next milestone",
    projectSlug: MOCK_PROJECT_SLUG,
    model: "anthropic/claude-sonnet-4-6",
    createdAt: Date.now() - 3_600_000,
    updatedAt: Date.now() - 600_000,
  },
  {
    id: "s2",
    title: "Debug SSE reconnection",
    projectSlug: MOCK_PROJECT_SLUG,
    model: "openai/gpt-4o",
    createdAt: Date.now() - 86_400_000,
    updatedAt: Date.now() - 86_400_000,
  },
];

const mockMessages = [
  {
    id: "m1",
    role: "user" as const,
    content: "Show me my epics",
    createdAt: Date.now() - 300_000,
  },
  {
    id: "m2",
    role: "assistant" as const,
    content:
      "Here are your current epics:\n\n1. **Platform Foundation** - Core infrastructure\n2. **UX Polish** - Interface improvements\n3. **Authentication** - Auth system",
    toolCalls: [{ name: "epic_list" }],
    createdAt: Date.now() - 295_000,
  },
];

// ---------------------------------------------------------------------------
// Chat store seeding decorator
// ---------------------------------------------------------------------------

/**
 * Seeds useChatStore and projectStore before rendering the ChatPage.
 * Resets state on unmount so stories don't leak into each other.
 */
function ChatStoreSeeder({
  sessions,
  messagesBySession,
  activeSessionId,
  children,
}: {
  sessions: typeof mockSessions;
  messagesBySession: Record<string, typeof mockMessages>;
  activeSessionId: string | null;
  children: React.ReactNode;
}) {
  useEffect(() => {
    // Seed project store
    projectStore.setState({
      projects: [MOCK_PROJECT],
      selectedProjectId: MOCK_PROJECT.id,
    });

    // Seed chat store
    useChatStore.setState({
      sessions,
      messagesBySession,
      activeSessionId,
      streamingBySession: {},
      loadingBySession: {},
      thinkingStartTimeBySession: {},
    });

    return () => {
      // Reset on unmount
      useChatStore.setState({
        sessions: [],
        messagesBySession: {},
        streamingBySession: {},
        loadingBySession: {},
        thinkingStartTimeBySession: {},
        activeSessionId: null,
      });
      projectStore.setState({
        projects: [],
        selectedProjectId: null,
      });
    };
  }, [sessions, messagesBySession, activeSessionId]);

  return <>{children}</>;
}

// ---------------------------------------------------------------------------
// Meta
// ---------------------------------------------------------------------------

const meta = {
  title: "Pages",
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

// ---------------------------------------------------------------------------
// KanbanPage stories
// ---------------------------------------------------------------------------

export const KanbanPagePopulated = {
  render: () => (
    <MemoryRouter initialEntries={["/"]}>
      <div className="h-screen p-4">
        <KanbanBoard
          tasks={tasksFixture}
          epics={new Map(epicsFixture.map((epic) => [epic.id, epic]))}
          disableSearchParamSync
        />
      </div>
    </MemoryRouter>
  ),
};

export const KanbanPageEmpty = {
  render: () => (
    <MemoryRouter initialEntries={["/"]}>
      <div className="h-screen p-4">
        <KanbanBoard tasks={[]} epics={new Map()} disableSearchParamSync />
      </div>
    </MemoryRouter>
  ),
};

// ---------------------------------------------------------------------------
// ChatPage stories
// ---------------------------------------------------------------------------

export const ChatPageWithConversation = {
  render: () => (
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={["/chat"]}>
        <ChatStoreSeeder
          sessions={mockSessions}
          messagesBySession={{ s1: mockMessages }}
          activeSessionId="s1"
        >
          <div className="flex h-screen">
            <ChatPage />
          </div>
        </ChatStoreSeeder>
      </MemoryRouter>
    </QueryClientProvider>
  ),
};

export const ChatPageEmpty = {
  render: () => (
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={["/chat"]}>
        <ChatStoreSeeder sessions={[]} messagesBySession={{}} activeSessionId={null}>
          <div className="flex h-screen">
            <ChatPage />
          </div>
        </ChatStoreSeeder>
      </MemoryRouter>
    </QueryClientProvider>
  ),
};
