import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { TaskSessionPage } from "./TaskSessionPage";
import { projectStore } from "@/stores/projectStore";
import { taskStore } from "@/stores/taskStore";
import { setSessionMessages } from "@/storybook-mocks/useSessionMessages";
import type { Project, Task } from "@/api/types";
import type { TimelineEntry, SessionInfo } from "@/hooks/useSessionMessages";

// ── Mock data ────────────────────────────────────────────────────────────────

const now = new Date();
const minutesAgo = (m: number) => new Date(now.getTime() - m * 60_000).toISOString();

const TASK_ID = "019cbe9f-6ae7-7d90-a8be-6ba626cc0119";

const mockTask: Task = {
  id: TASK_ID,
  short_id: "j4m1",
  title: "Implement SSE reconnection with exponential backoff",
  description:
    "Handle SSE disconnections gracefully with exponential backoff and jitter.\n\nThe reconnection manager should track attempts and provide status feedback to the UI layer.",
  design: "Use a reconnection manager that tracks attempts...",
  acceptance_criteria: [
    { criterion: "SSE reconnects automatically on disconnect", met: true },
    { criterion: "Backoff uses exponential delay with jitter", met: true },
    { criterion: "Max reconnection attempts is configurable", met: false },
    { criterion: "Connection status shown in UI", met: false },
  ],
  issue_type: "task",
  status: "in_progress",
  priority: 1,
  epic_id: "epic-foundation",
  labels: ["sse", "reliability"],
  memory_refs: [],
  owner: "fernando",
  created_at: minutesAgo(60 * 72),
  updated_at: minutesAgo(60),
  duration_seconds: 2400,
  session_count: 3,
  active_session: { started_at: minutesAgo(12), model_id: "claude-sonnet-4-6" },
  reopen_count: 1,
  continuation_count: 0,
};

const closedTask: Task = {
  ...mockTask,
  status: "closed",
  active_session: null,
  acceptance_criteria: [
    { criterion: "SSE reconnects automatically on disconnect", met: true },
    { criterion: "Backoff uses exponential delay with jitter", met: true },
    { criterion: "Max reconnection attempts is configurable", met: true },
    { criterion: "Connection status shown in UI", met: true },
  ],
  duration_seconds: 4800,
  reopen_count: 1,
};

const mockSessions: SessionInfo[] = [
  {
    id: "sess-001",
    agentType: "worker",
    modelId: "claude-sonnet-4-6",
    startedAt: minutesAgo(60),
    endedAt: minutesAgo(35),
    status: "completed",
    tokensIn: 45200,
    tokensOut: 12800,
    cacheReadTokens: 38000,
    cacheWriteTokens: 7200,
  },
  {
    id: "sess-002",
    agentType: "reviewer",
    modelId: "claude-sonnet-4-6",
    startedAt: minutesAgo(30),
    endedAt: minutesAgo(22),
    status: "completed",
    tokensIn: 18400,
    tokensOut: 5600,
    cacheReadTokens: 14000,
    cacheWriteTokens: 3100,
  },
  {
    id: "sess-003",
    agentType: "worker",
    modelId: "claude-sonnet-4-6",
    startedAt: minutesAgo(12),
    status: "running",
    tokensIn: 32100,
    tokensOut: 8900,
    cacheReadTokens: 27500,
    cacheWriteTokens: 4800,
  },
];

const completedSessions: SessionInfo[] = mockSessions.map((s) => ({
  ...s,
  status: "completed",
  endedAt: s.endedAt ?? minutesAgo(5),
}));

const baseTimeline: TimelineEntry[] = [
  {
    kind: "divider",
    label: "Open → Coding",
    timestamp: minutesAgo(62),
  },
  {
    kind: "message",
    role: "user",
    content: [
      {
        type: "text",
        text: "Implement SSE reconnection with exponential backoff. The connection should recover automatically when the server restarts.",
      },
    ],
    sessionId: "sess-001",
    agentType: "worker",
    modelId: "claude-sonnet-4-6",
    timestamp: minutesAgo(60),
  },
  {
    kind: "message",
    role: "assistant",
    content: [
      {
        type: "text",
        text: "I'll implement the SSE reconnection logic. Let me start by examining the current SSE connection code to understand the existing architecture.",
      },
    ],
    sessionId: "sess-001",
    agentType: "worker",
    modelId: "claude-sonnet-4-6",
    timestamp: minutesAgo(58),
  },
  {
    kind: "message",
    role: "assistant",
    content: [
      {
        type: "tool_use",
        name: "read_file",
        input: { path: "src/stores/sseStore.ts" },
      },
    ],
    sessionId: "sess-001",
    agentType: "worker",
    modelId: "claude-sonnet-4-6",
    timestamp: minutesAgo(57),
  },
  {
    kind: "message",
    role: "assistant",
    content: [
      {
        type: "text",
        text: "I've added exponential backoff with jitter to the SSE reconnection logic. The implementation:\n\n1. **Base delay** starts at 1s, doubles each attempt up to 30s max\n2. **Jitter** adds ±25% randomization to prevent thundering herd\n3. **Reset** on successful reconnection\n\n```typescript\nconst delay = Math.min(BASE_DELAY * 2 ** attempt, MAX_DELAY);\nconst jitter = delay * (0.75 + Math.random() * 0.5);\n```\n\nThe connection status is exposed via `sseStore.connectionStatus` for the UI to consume.",
      },
    ],
    sessionId: "sess-001",
    agentType: "worker",
    modelId: "claude-sonnet-4-6",
    timestamp: minutesAgo(40),
  },
  {
    kind: "divider",
    label: "Coding → Review",
    timestamp: minutesAgo(36),
  },
  {
    kind: "command",
    name: "test",
    body: "PASS src/stores/sseStore.test.ts (4 tests)\nPASS src/hooks/useSSE.test.ts (2 tests)\n\nTest Suites: 2 passed, 2 total\nTests:       6 passed, 6 total",
    passed: true,
    timestamp: minutesAgo(35),
  },
  {
    kind: "divider",
    label: "Review started",
    timestamp: minutesAgo(32),
  },
  {
    kind: "message",
    role: "assistant",
    content: [
      {
        type: "text",
        text: "The reconnection implementation looks solid. Two concerns:\n\n1. The max retry count isn't configurable — it's hardcoded to 10. Consider exposing this via project settings.\n2. The UI connection status badge is missing from the board header.\n\nReopening for these fixes.",
      },
    ],
    sessionId: "sess-002",
    agentType: "reviewer",
    modelId: "claude-sonnet-4-6",
    timestamp: minutesAgo(25),
  },
  {
    kind: "divider",
    label: "Review → Coding — reviewer requested changes",
    timestamp: minutesAgo(24),
  },
  {
    kind: "message",
    role: "user",
    content: [
      {
        type: "text",
        text: "Reviewer flagged two issues: make max retries configurable and add connection status badge to the board header.",
      },
    ],
    sessionId: "sess-003",
    agentType: "worker",
    modelId: "claude-sonnet-4-6",
    timestamp: minutesAgo(12),
  },
  {
    kind: "message",
    role: "assistant",
    content: [
      {
        type: "text",
        text: "Addressing the reviewer's feedback. I'll make the max retry count configurable through project settings and add a connection status indicator to the board header.",
      },
    ],
    sessionId: "sess-003",
    agentType: "worker",
    modelId: "claude-sonnet-4-6",
    timestamp: minutesAgo(10),
  },
];

// ── Story state seeding ──────────────────────────────────────────────────────
//
// `vi.mock` crashes under `storybook dev` (Vite builder). Instead we drive the
// page through the REAL zustand stores (seeded via `setState`) and the aliased
// `@/hooks/useSessionMessages` mock (see `.storybook/main.ts` +
// src/storybook-mocks). Storybook does a full page reload between stories, so
// this module-level state resets each time.
//
// The page reads only `useSelectedProject` (for the projectSlug), `useTaskStore`
// / `taskStore` (for the task), and `useSessionMessages` — it does not touch the
// epic store or the window shim, so those former mocks are dropped.

const project: Project = {
  id: "project-djinn",
  name: "djinnos/djinn",
  github_owner: "djinnos",
  github_repo: "djinn",
};

function applyStory(opts: {
  task: Task | null;
  timeline?: TimelineEntry[];
  sessions?: SessionInfo[];
  loading?: boolean;
  streamingText?: Map<string, string>;
}) {
  projectStore.setState({ projects: [project], selectedProjectId: project.id });

  const tasks = new Map<string, Task>();
  if (opts.task) tasks.set(opts.task.id, opts.task);
  taskStore.setState({ tasks });

  setSessionMessages({
    timeline: opts.timeline ?? [],
    sessions: opts.sessions ?? [],
    loading: opts.loading ?? false,
    streamingText: opts.streamingText ?? new Map(),
  });
}

// ── Story wrapper ────────────────────────────────────────────────────────────

function TaskSessionStory() {
  return (
    <MemoryRouter initialEntries={[`/task/${TASK_ID}`]}>
      <Routes>
        <Route path="/task/:taskId" element={<TaskSessionPage />} />
      </Routes>
    </MemoryRouter>
  );
}

const meta: Meta<typeof TaskSessionStory> = {
  title: "Session/TaskSessionPage",
  component: TaskSessionStory,
  parameters: { layout: "fullscreen" },
};

export default meta;
type Story = StoryObj<typeof TaskSessionStory>;

// ── Stories ──────────────────────────────────────────────────────────────────

export const ActiveSession: Story = {
  beforeEach: () => {
    applyStory({
      task: mockTask,
      timeline: baseTimeline,
      sessions: mockSessions,
      streamingText: new Map([
        ["sess-003", "I've updated the `MAX_RETRIES` to be configurable via `project_config_set`. Now working on the connection status badge..."],
      ]),
    });
  },
};

export const CompletedTask: Story = {
  beforeEach: () => {
    applyStory({
      task: closedTask,
      sessions: completedSessions,
      timeline: [
        ...baseTimeline,
      {
        kind: "message" as const,
        role: "assistant" as const,
        content: [
          {
            type: "text",
            text: "Both issues resolved:\n\n1. Max retries is now configurable via `project_config_set max_sse_retries <n>`\n2. Added a `ConnectionStatusBadge` component to the board header that shows connected/reconnecting/disconnected states\n\nAll tests passing. Ready for re-review.",
          },
        ],
        sessionId: "sess-003",
        agentType: "worker",
        modelId: "claude-sonnet-4-6",
        timestamp: minutesAgo(4),
      },
      {
        kind: "command" as const,
        name: "test",
        body: "PASS src/stores/sseStore.test.ts (6 tests)\nPASS src/hooks/useSSE.test.ts (3 tests)\nPASS src/components/ConnectionStatusBadge.test.ts (2 tests)\n\nTest Suites: 3 passed, 3 total\nTests:       11 passed, 11 total",
        passed: true,
        timestamp: minutesAgo(3),
      },
      {
        kind: "divider" as const,
        label: "Review → Done",
        timestamp: minutesAgo(2),
      },
      ],
    });
  },
};

export const Loading: Story = {
  beforeEach: () => {
    applyStory({ task: mockTask, loading: true });
  },
};

export const TaskNotFound: Story = {
  beforeEach: () => {
    applyStory({ task: null });
  },
};
