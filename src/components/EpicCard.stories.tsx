import { EpicCard } from "./EpicCard";
import type { Epic, Task } from "@/types";

const makeEpic = (id: string, title: string, priority: Epic["priority"]): Epic => ({
  id,
  title,
  description: "Epic description",
  priority,
  status: "active",
  labels: [],
  owner: null,
  createdAt: new Date("2026-01-01").toISOString(),
  updatedAt: new Date("2026-01-01").toISOString(),
});

const makeTask = (id: string, epicId: string, title: string, status: Task["status"]): Task => ({
  id,
  epicId,
  title,
  description: "Story description",
  design: "",
  acceptanceCriteria: [],
  activity: [],
  status,
  priority: "P2",
  labels: [],
  owner: null,
  createdAt: new Date("2026-01-01").toISOString(),
  updatedAt: new Date("2026-01-01").toISOString(),
});

const meta = {
  title: "Roadmap/EpicCard",
  component: EpicCard,
  parameters: { layout: "padded" },
} as const;

export default meta;

const epic = makeEpic("epic-1", "Storybook Epic", "P1");

export const Progress0Collapsed = {
  args: {
    epic,
    mockTasks: [
      makeTask("t1", epic.id, "Task 1", "pending"),
      makeTask("t2", epic.id, "Task 2", "in_progress"),
    ],
    defaultExpanded: false,
  },
};

export const Progress50Expanded = {
  args: {
    epic: makeEpic("epic-2", "Half Done Epic", "P2"),
    mockTasks: [
      makeTask("t3", "epic-2", "Task 1", "completed"),
      makeTask("t4", "epic-2", "Task 2", "pending"),
    ],
    defaultExpanded: true,
  },
};

export const Progress100ExpandedManyTasks = {
  args: {
    epic: makeEpic("epic-3", "Completed Epic", "P0"),
    mockTasks: [
      makeTask("t5", "epic-3", "Task 1", "completed"),
      makeTask("t6", "epic-3", "Task 2", "completed"),
      makeTask("t7", "epic-3", "Task 3", "completed"),
      makeTask("t8", "epic-3", "Task 4", "completed"),
      makeTask("t9", "epic-3", "Task 5", "completed"),
    ],
    defaultExpanded: true,
  },
};
