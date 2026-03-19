import { SessionThread } from "./SessionThread";
import type {
  TimelineEntry,
  ChatMessage,
  SystemDivider,
  CommandBlock,
} from "@/hooks/useSessionMessages";

export default {
  title: "Components/SessionThread",
  component: SessionThread,
  parameters: { layout: "fullscreen" },
  decorators: [
    (Story: React.ComponentType) => (
      <div className="h-[600px] w-full bg-background text-foreground">
        <Story />
      </div>
    ),
  ],
};

// ── Helpers ──────────────────────────────────────────────────────────────────

const now = new Date();
const minutesAgo = (m: number) => new Date(now.getTime() - m * 60_000).toISOString();

function msg(
  role: ChatMessage["role"],
  agentType: string,
  content: ChatMessage["content"],
  extra?: Partial<ChatMessage>,
): ChatMessage {
  return {
    kind: "message",
    role,
    agentType,
    content,
    sessionId: extra?.sessionId ?? "ses-001",
    modelId: extra?.modelId ?? "claude-sonnet-4-6",
    timestamp: extra?.timestamp ?? minutesAgo(5),
  };
}

function divider(label: string, ts?: string): SystemDivider {
  return { kind: "divider", label, timestamp: ts ?? minutesAgo(3) };
}

function command(name: string, passed: boolean, body = "", ts?: string): CommandBlock {
  return { kind: "command", name, body, passed, timestamp: ts ?? minutesAgo(2) };
}

// ── Stories ──────────────────────────────────────────────────────────────────

export const Loading = {
  args: {
    timeline: [],
    streamingText: new Map(),
    loading: true,
    error: null,
  },
};

export const Error = {
  args: {
    timeline: [],
    streamingText: new Map(),
    loading: false,
    error: "Failed to load session history: connection refused on port 8372",
  },
};

export const Empty = {
  args: {
    timeline: [],
    streamingText: new Map(),
    loading: false,
    error: null,
  },
};

const workerTimeline: TimelineEntry[] = [
  msg("user", "worker", [
    { type: "text", text: "Implement drag-and-drop reordering for the kanban board columns." },
  ], { timestamp: minutesAgo(12) }),
  msg("assistant", "worker", [
    { type: "text", text: "I'll start by reading the existing KanbanBoard component to understand the current structure." },
    { type: "tool_use", name: "Read", input: { file_path: "src/components/KanbanBoard.tsx" } },
  ], { timestamp: minutesAgo(11) }),
  msg("assistant", "worker", [
    {
      type: "tool_result",
      tool_use_id: "tu-001",
      content: "import { DndContext } from '@dnd-kit/core';\nimport { SortableContext } from '@dnd-kit/sortable';\n// ... 142 lines",
    },
  ], { timestamp: minutesAgo(10) }),
  msg("assistant", "worker", [
    { type: "text", text: "The component already imports `@dnd-kit/core`. I'll add column-level sortable contexts and wire up the `onDragEnd` handler.\n\n```tsx\nconst handleDragEnd = (event: DragEndEvent) => {\n  const { active, over } = event;\n  if (!over || active.id === over.id) return;\n  moveColumn(active.id, over.id);\n};\n```" },
    { type: "tool_use", name: "Edit", input: { file_path: "src/components/KanbanBoard.tsx", old_string: "// column drag", new_string: "handleDragEnd logic" } },
  ], { timestamp: minutesAgo(8) }),
  msg("assistant", "worker", [
    { type: "text", text: "Column drag-and-drop is now implemented. The `SortableContext` wraps the column list and persists order to the store." },
  ], { timestamp: minutesAgo(6) }),
];

export const WorkerConversation = {
  args: {
    timeline: workerTimeline,
    streamingText: new Map(),
    loading: false,
    error: null,
  },
};

const multiAgentTimeline: TimelineEntry[] = [
  msg("user", "worker", [
    { type: "text", text: "Fix the SSE reconnection logic that drops events after server restart." },
  ], { sessionId: "ses-w1", timestamp: minutesAgo(20) }),
  msg("assistant", "worker", [
    { type: "text", text: "Looking at the SSE store to find the reconnection handler." },
    { type: "tool_use", name: "Grep", input: { pattern: "reconnect", path: "src/stores/sseStore.ts" } },
  ], { sessionId: "ses-w1", timestamp: minutesAgo(19) }),
  msg("assistant", "worker", [
    { type: "text", text: "Found the issue: the `lastEventId` is not sent on reconnect. I'll add it to the headers.\n\nThe fix adds `Last-Event-ID` to the EventSource init so the server can replay missed events." },
    { type: "tool_use", name: "Edit", input: { file_path: "src/stores/sseStore.ts", old_string: "new EventSource(url)", new_string: "new EventSource(url, { headers })" } },
  ], { sessionId: "ses-w1", timestamp: minutesAgo(16) }),
  msg("assistant", "worker", [
    { type: "text", text: "SSE reconnection now includes `Last-Event-ID`. Events won't be dropped on server restart." },
  ], { sessionId: "ses-w1", timestamp: minutesAgo(14) }),

  divider("Worker completed — Review started", minutesAgo(13)),

  msg("user", "task_reviewer", [
    { type: "text", text: "Review the changes made to fix SSE reconnection logic." },
  ], { sessionId: "ses-r1", timestamp: minutesAgo(12) }),
  msg("assistant", "task_reviewer", [
    { type: "text", text: "Reviewing the diff. The `Last-Event-ID` header addition looks correct.\n\n**Findings:**\n- The reconnect delay uses exponential backoff (good)\n- Missing: the `readyState` check before scheduling reconnect could cause duplicate connections\n- Suggest adding a guard: `if (source.readyState !== EventSource.CONNECTING)`" },
  ], { sessionId: "ses-r1", timestamp: minutesAgo(10) }),

  divider("Review complete — PM summary", minutesAgo(9)),

  msg("assistant", "pm", [
    { type: "text", text: "SSE reconnection fix approved with one suggestion for a guard clause. Task moved to **done**. Epic progress: 5/8 tasks complete." },
  ], { sessionId: "ses-pm1", timestamp: minutesAgo(8) }),
];

export const MultiAgentThread = {
  args: {
    timeline: multiAgentTimeline,
    streamingText: new Map(),
    loading: false,
    error: null,
    activeAgentType: "task_reviewer",
  },
};

const verificationTimeline: TimelineEntry[] = [
  msg("assistant", "worker", [
    { type: "text", text: "Implementation complete. Running verification commands." },
  ], { timestamp: minutesAgo(8) }),
  command("pnpm install --frozen-lockfile", true, "Lockfile is up to date, resolution step is skipped\nDependencies are already up to date\nDone in 1.2s", minutesAgo(7)),
  command("pnpm tsc --noEmit", true, "Done in 4.8s", minutesAgo(6)),
  command("pnpm test", false, [
    "FAIL src/stores/sseStore.test.ts",
    " \u2715 reconnects with Last-Event-ID header (42ms)",
    "",
    "  Expected: \"42\"",
    "  Received: undefined",
    "",
    "  at Object.<anonymous> (src/stores/sseStore.test.ts:87:31)",
    "",
    "Tests:  1 failed, 14 passed, 15 total",
  ].join("\n"), minutesAgo(5)),
  msg("assistant", "worker", [
    { type: "text", text: "The test expects the header to be a string but we're passing a number. Let me fix the type conversion." },
  ], { timestamp: minutesAgo(4) }),
  command("pnpm test", true, "Tests:  15 passed, 15 total\nDone in 8.9s", minutesAgo(3)),
  msg("assistant", "worker", [
    { type: "text", text: "All verification checks pass now." },
  ], { timestamp: minutesAgo(2) }),
];

export const WithVerificationCommands = {
  args: {
    timeline: verificationTimeline,
    streamingText: new Map(),
    loading: false,
    error: null,
  },
};

const streamingTimeline: TimelineEntry[] = [
  msg("user", "worker", [
    { type: "text", text: "Add a loading skeleton to the task detail panel." },
  ], { timestamp: minutesAgo(3) }),
  msg("assistant", "worker", [
    { type: "text", text: "I'll create a skeleton variant of the TaskDetailPanel that shows placeholder shapes while data loads." },
    { type: "tool_use", name: "Read", input: { file_path: "src/components/TaskDetailPanel.tsx" } },
  ], { timestamp: minutesAgo(2) }),
];

export const StreamingState = {
  args: {
    timeline: streamingTimeline,
    streamingText: new Map([
      ["ses-001", "I can see the component renders several sections. I'll add a `TaskDetailSkeleton` component that mirrors the layout with `Skeleton` primitives from shadcn:\n\n```tsx\nfunction TaskDetailSkeleton() {\n  return (\n    <div className=\"space-y-4 p-4\">\n      <Skeleton className=\"h-6 w-3/4\" />\n      <Skeleton className=\"h-4 w-1/2\" />"],
    ]),
    loading: false,
    error: null,
    activeAgentType: "worker",
  },
};
