import { TaskCard } from "./TaskCard";
import type { Epic, Task } from "@/types";

const now = new Date();
const minutesAgo = (m: number) => new Date(now.getTime() - m * 60_000).toISOString();

const activeEpic: Epic = {
  id: "epic-platform-foundation",
  title: "Platform Foundation",
  description: "Core infrastructure and tooling for Kanban workflows.",
  status: "active",
  priority: "P1",
  labels: ["platform", "kanban"],
  owner: "fernando",
  createdAt: minutesAgo(60 * 24 * 15),
  updatedAt: minutesAgo(90),
};

const completedEpic: Epic = {
  ...activeEpic,
  id: "epic-polish",
  title: "UX Polish and Accessibility",
  status: "completed",
};

const baseTask: Task = {
  id: "019cbe9f-6ae7-7d90-a8be-6ba626cc0119",
  shortId: "j4m1",
  title: "Add Storybook stories for TaskCard and TaskDetailPanel",
  description: "Create stories with realistic mock data for kanban components.",
  design: "Use pure component props with realistic task/epic entities.",
  acceptanceCriteria: [
    { criterion: "TaskCard stories show all priority levels", met: true },
    { criterion: "TaskCard stories show all status states", met: true },
  ],
  activity: ["Scope reviewed", "Story variants planned"],
  status: "pending",
  priority: "P1",
  epicId: activeEpic.id,
  labels: ["storybook", "kanban"],
  owner: "fernando",
  createdAt: minutesAgo(60 * 6),
  updatedAt: minutesAgo(30),
  trackedSeconds: 42 * 60,
  sessionCount: 2,
  reviewPhase: undefined,
  activeSessionStartedAt: null,
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
      <TaskCard task={{ ...baseTask, id: "task-p0", shortId: "p0a1", priority: "P0", title: "Critical SSE regression on board updates" }} epic={activeEpic} />
      <TaskCard task={{ ...baseTask, id: "task-p1", shortId: "p1a2", priority: "P1", title: "Ship task board keyboard navigation" }} epic={activeEpic} />
      <TaskCard task={{ ...baseTask, id: "task-p2", shortId: "p2a3", priority: "P2", title: "Improve task metadata formatting" }} epic={completedEpic} />
      <TaskCard task={{ ...baseTask, id: "task-p3", shortId: "p3a4", priority: "P3", title: "Refine empty-state copy" }} />
    </div>
  ),
};

export const StatusStates = {
  render: () => (
    <div className="grid max-w-5xl grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-4">
      <TaskCard task={{ ...baseTask, id: "task-pending", shortId: "st01", status: "pending", title: "Pending: collect acceptance criteria" }} epic={activeEpic} />
      <TaskCard
        task={{
          ...baseTask,
          id: "task-running",
          shortId: "st02",
          status: "in_progress",
          title: "In Progress: implement Storybook task stories",
          trackedSeconds: 8 * 60,
          activeSessionStartedAt: minutesAgo(12),
          sessionCount: 1,
          sessionModelId: "chatgpt_codex/gpt-5.3-codex",
        }}
        epic={activeEpic}
      />
      <TaskCard
        task={{
          ...baseTask,
          id: "task-completed",
          shortId: "st03",
          status: "completed",
          title: "Completed: add board filter chips",
          trackedSeconds: 70 * 60,
          sessionCount: 4,
          reviewPhase: "needs_task_review",
        }}
        epic={completedEpic}
      />
      <TaskCard
        task={{
          ...baseTask,
          id: "task-blocked",
          shortId: "st04",
          status: "blocked",
          title: "Blocked: waiting for API contract confirmation",
          owner: null,
          epicId: null,
          reviewPhase: "in_task_review",
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
      shortId: "long",
      title:
        "Design and implement comprehensive story scenarios for kanban cards including running state timer, review indicators, owner fallback avatar initials, and robust truncation behavior for exceptionally long task names",
      status: "pending",
      owner: null,
      epicId: null,
      trackedSeconds: 0,
      sessionCount: 0,
    },
  },
};
