import { useEffect } from "react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ChatPage } from "@/pages/ChatPage";
import { useChatStore } from "@/stores/chatStore";
import { projectStore } from "@/stores/useProjectStore";

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
//
// Page-level composition of the chat surface. Unlike Chat/ChatView (which
// renders the ChatView component in isolation), this exercises the full
// ChatPage shell: the store-driven ChatSessionList sidebar alongside the
// conversation view.

const meta = {
  title: "Chat/ChatPage",
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

export const WithConversation = {
  name: "Chat Page",
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
