import { TaskDetailPanel } from "./TaskDetailPanel";
import type { Epic, Task } from "@/api/types";

const now = new Date();
const hoursAgo = (h: number) => new Date(now.getTime() - h * 60 * 60 * 1000).toISOString();

const epic: Epic = {
  id: "epic-kanban-quality",
  short_id: "eq01",
  title: "Kanban Quality Improvements",
  description: "Raise task board UX quality through better observability and documentation.",
  emoji: "⚡",
  color: "#F59E0B",
  status: "active",
  owner: "fernando",
  created_at: hoursAgo(24 * 14),
  updated_at: hoursAgo(5),
};

const detailedTask: Task = {
  id: "019cbe9f-6ae7-7d90-a8be-6ba626cc0119",
  short_id: "j4m1",
  title: "Add Storybook stories for TaskCard and TaskDetailPanel",
  description: `Create rich Storybook coverage for the kanban card and detail panel.`,
  design: `Use static mock entities matching real server payload shape.`,
  acceptance_criteria: [
    { criterion: "TaskCard stories show all priority levels", met: true },
    { criterion: "TaskCard stories show all status states", met: true },
    { criterion: "TaskDetailPanel story shows full detail view", met: true },
    { criterion: "Stories use realistic mock data", met: true },
  ],
  issue_type: "task",
  status: "in_progress",
  priority: 1,
  epic_id: epic.id,
  labels: ["storybook", "kanban", "ui"],
  memory_refs: [],
  owner: "fernando",
  created_at: hoursAgo(9),
  updated_at: hoursAgo(1),
  duration_seconds: 64 * 60,
  session_count: 3,
  active_session: { started_at: hoursAgo(0.5), model_id: "chatgpt_codex/gpt-5.3-codex" },
  reopen_count: 1,
  continuation_count: 0,
};

const meta = {
  title: "Kanban/TaskDetailPanel",
  component: TaskDetailPanel,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

export const FullDetailView = {
  args: {
    open: true,
    task: detailedTask,
    epic,
    onClose: () => {},
  },
};
