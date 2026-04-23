import { useEffect } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { useChatStore } from "@/stores/chatStore";
import { projectStore } from "@/stores/projectStore";
import { ChatView } from "./ChatView";

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, staleTime: Infinity } },
});

// Pre-seed model data so ChatView doesn't need a real API
queryClient.setQueryData(["provider-models-connected"], [
  { id: "anthropic/claude-sonnet-4-6", name: "Claude Sonnet 4.6", provider_id: "anthropic" },
  { id: "openai/gpt-4o", name: "GPT-4o", provider_id: "openai" },
]);

function ChatViewSeeded({
  withMessages = false,
}: {
  withMessages?: boolean;
}) {
  useEffect(() => {
    projectStore.setState({
      projects: [{ id: "proj-1", name: "DjinnOS Desktop", github_owner: "djinnos", github_repo: "desktop" }],
      selectedProjectId: "proj-1",
    });

    if (withMessages) {
      const sessionId = useChatStore.getState().createSession("djinnos/desktop", "anthropic/claude-sonnet-4-6");
      useChatStore.getState().setActiveSession(sessionId);
      useChatStore.getState().addMessage(sessionId, {
        id: "m1",
        role: "user",
        content: "Show me my epics and their progress",
        createdAt: Date.now() - 60000,
      });
      useChatStore.getState().addMessage(sessionId, {
        id: "m2",
        role: "assistant",
        content:
          "Here are your current epics:\n\n| Epic | Status | Tasks |\n|------|--------|-------|\n| Platform Foundation | Active | 12/18 done |\n| UX Polish | Active | 4/8 done |\n| Authentication | Active | 6/9 done |\n\nOverall progress is **72%** across all epics.",
        toolCalls: [{ name: "epic_list" }, { name: "task_count" }],
        createdAt: Date.now() - 55000,
      });
      useChatStore.getState().addMessage(sessionId, {
        id: "m3",
        role: "user",
        content: "What tasks are blocked?",
        createdAt: Date.now() - 30000,
      });
      useChatStore.getState().addMessage(sessionId, {
        id: "m4",
        role: "assistant",
        content:
          "There are **2 blocked tasks**:\n\n1. **Keyboard shortcut manager** (P1) — waiting on `focus-trap` library decision\n2. **Token refresh logic** (P0) — blocked by auth provider API changes\n\nBoth are in the Open column on your board.",
        toolCalls: [{ name: "task_blocked_list" }],
        createdAt: Date.now() - 25000,
      });
      useChatStore.getState().updateSessionTitle(sessionId, "Epic progress overview");
    }

    return () => {
      useChatStore.setState({
        sessions: [],
        messagesBySession: {},
        streamingBySession: {},
        loadingBySession: {},
        thinkingStartTimeBySession: {},
        activeSessionId: null,
      });
    };
  }, [withMessages]);

  return (
    <QueryClientProvider client={queryClient}>
      <ChatView />
    </QueryClientProvider>
  );
}

const meta = {
  title: "Chat/ChatView",
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

export const EmptyState = {
  name: "ChatView / Empty",
  render: () => (
    <div className="flex h-screen">
      <ChatViewSeeded />
    </div>
  ),
};

export const WithConversation = {
  name: "ChatView / With Conversation",
  render: () => (
    <div className="flex h-screen">
      <ChatViewSeeded withMessages />
    </div>
  ),
};
