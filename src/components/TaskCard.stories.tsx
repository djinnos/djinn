import { TaskCard } from "./TaskCard";
import type { Epic, Task } from "@/api/types";

const now = new Date();
const minutesAgo = (m: number) => new Date(now.getTime() - m * 60_000).toISOString();

const activeEpic: Epic = {
  id: "epic-platform-foundation",
  short_id: "ep01",
  title: "Platform Foundation",
  description: "Core infrastructure and tooling for Kanban workflows.",
  emoji: "🚀",
  color: "#3B82F6",
  status: "active",
  owner: "fernando",
  created_at: minutesAgo(60 * 24 * 15),
  updated_at: minutesAgo(90),
};

const completedEpic: Epic = {
  ...activeEpic,
  id: "epic-polish",
  short_id: "ep02",
  title: "UX Polish and Accessibility",
  status: "closed",
};

const baseTask: Task = {
  id: "019cbe9f-6ae7-7d90-a8be-6ba626cc0119",
  short_id: "j4m1",
  title: "Add Storybook stories for TaskCard and TaskDetailPanel",
  description: "Create stories with realistic mock data for kanban components.",
  design: "Use pure component props with realistic task/epic entities.",
  acceptance_criteria: [
    { criterion: "TaskCard stories show all priority levels", met: true },
    { criterion: "TaskCard stories show all status states", met: true },
  ],
  issue_type: "task",
  status: "open",
  priority: 1,
  epic_id: activeEpic.id,
  labels: ["storybook", "kanban"],
  memory_refs: [],
  owner: "fernando",
  created_at: minutesAgo(60 * 6),
  updated_at: minutesAgo(30),
  duration_seconds: 42 * 60,
  session_count: 2,
  reopen_count: 0,
  continuation_count: 0,
  active_session: null,
};

const meta = {
  title: "Kanban/TaskCard",
  component: TaskCard,
  parameters: {
    layout: "padded",
  },
};

export default meta;

export const PriorityLevels = {
  render: () => (
    <div className="grid max-w-5xl grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-4">
      <TaskCard task={{ ...baseTask, id: "task-p0", short_id: "p0a1", priority: 0, title: "Critical SSE regression on board updates" }} epic={activeEpic} />
      <TaskCard task={{ ...baseTask, id: "task-p1", short_id: "p1a2", priority: 1, title: "Ship task board keyboard navigation" }} epic={activeEpic} />
      <TaskCard task={{ ...baseTask, id: "task-p2", short_id: "p2a3", priority: 2, title: "Improve task metadata formatting" }} epic={completedEpic} />
      <TaskCard task={{ ...baseTask, id: "task-p3", short_id: "p3a4", priority: 3, title: "Refine empty-state copy" }} />
    </div>
  ),
};

export const StatusStates = {
  render: () => (
    <div className="grid max-w-5xl grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-3">
      <TaskCard task={{ ...baseTask, id: "task-pending", short_id: "st01", status: "open", title: "Pending: collect acceptance criteria" }} epic={activeEpic} />
      <TaskCard
        task={{
          ...baseTask,
          id: "task-running",
          short_id: "st02",
          status: "in_progress",
          title: "In Progress: implement Storybook task stories",
          duration_seconds: 8 * 60,
          active_session: { started_at: minutesAgo(12), model_id: "chatgpt_codex/gpt-5.3-codex" },
          session_count: 1,
        }}
        epic={activeEpic}
      />
      <TaskCard
        task={{
          ...baseTask,
          id: "task-needs-review",
          short_id: "st03",
          status: "needs_task_review",
          title: "Needs Review: validate SSE reconnection logic",
          duration_seconds: 35 * 60,
          session_count: 2,
        }}
        epic={activeEpic}
      />
      <TaskCard
        task={{
          ...baseTask,
          id: "task-in-review",
          short_id: "st04",
          status: "in_task_review",
          title: "In Review: agent reviewing kanban drag-and-drop",
          duration_seconds: 55 * 60,
          active_session: { started_at: minutesAgo(8), model_id: "claude-sonnet-4-6" },
          session_count: 3,
        }}
        epic={activeEpic}
      />
      <TaskCard
        task={{
          ...baseTask,
          id: "task-completed",
          short_id: "st05",
          status: "closed",
          title: "Completed: add board filter chips",
          duration_seconds: 70 * 60,
          session_count: 4,
        }}
        epic={completedEpic}
      />
      <TaskCard
        task={{
          ...baseTask,
          id: "task-blocked",
          short_id: "st06",
          status: "open",
          title: "Blocked: waiting for API contract confirmation",
          owner: null,
          epic_id: undefined,
          duration_seconds: 42 * 60,
          session_count: 3,
          unresolved_blocker_count: 2,
        }}
      />
    </div>
  ),
};

export const LongTitleNoEpicUnassigned = {
  args: {
    task: {
      ...baseTask,
      id: "task-long-title",
      short_id: "long",
      title:
        "Design and implement comprehensive story scenarios for kanban cards including running state timer, review indicators, owner fallback avatar initials, and robust truncation behavior for exceptionally long task names",
      status: "open",
      owner: null,
      epic_id: undefined,
      duration_seconds: 0,
      session_count: 0,
    },
  },
};
