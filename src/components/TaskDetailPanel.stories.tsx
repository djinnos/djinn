import { TaskDetailPanel } from "./TaskDetailPanel";
import type { Epic, Task } from "@/types";

const now = new Date();
const hoursAgo = (h: number) => new Date(now.getTime() - h * 60 * 60 * 1000).toISOString();

const epic: Epic = {
  id: "epic-kanban-quality",
  title: "Kanban Quality Improvements",
  description: "Raise task board UX quality through better observability and documentation.",
  status: "active",
  priority: "P1",
  labels: ["kanban", "ux", "storybook"],
  owner: "fernando",
  createdAt: hoursAgo(24 * 14),
  updatedAt: hoursAgo(5),
};

const detailedTask: Task = {
  id: "019cbe9f-6ae7-7d90-a8be-6ba626cc0119",
  shortId: "j4m1",
  title: "Add Storybook stories for TaskCard and TaskDetailPanel",
  description: `Create rich Storybook coverage for the kanban card and detail panel.`,
  design: `Use static mock entities matching real server payload shape.`,
  acceptanceCriteria: [
    { criterion: "TaskCard stories show all priority levels", met: true },
    { criterion: "TaskCard stories show all status states", met: true },
    { criterion: "TaskDetailPanel story shows full detail view", met: true },
    { criterion: "Stories use realistic mock data", met: true },
  ],
  activity: [
    "[15:20] Reviewed existing TaskCard and TaskDetailPanel props",
    "[15:27] Added TaskCard Storybook variants",
    "[15:34] Added detailed TaskDetailPanel story",
  ],
  status: "in_progress",
  priority: "P1",
  epicId: epic.id,
  labels: ["storybook", "kanban", "ui"],
  owner: "fernando",
  createdAt: hoursAgo(9),
  updatedAt: hoursAgo(1),
  trackedSeconds: 64 * 60,
  sessionCount: 3,
  activeSessionStartedAt: hoursAgo(0.5),
  sessionModelId: "chatgpt_codex/gpt-5.3-codex",
  reopenCount: 1,
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
