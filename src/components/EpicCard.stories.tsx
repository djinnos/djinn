import { EpicCard } from "./EpicCard";
import type { Epic, Task } from "@/api/types";

const makeEpic = (id: string, title: string): Epic => ({
  id,
  short_id: id.slice(0, 4),
  title,
  description: "Epic description",
  emoji: "🚀",
  color: "#3B82F6",
  status: "active",
  owner: null,
  created_at: new Date("2026-01-01").toISOString(),
  updated_at: new Date("2026-01-01").toISOString(),
});

const makeTask = (id: string, epicId: string, title: string, status: string): Task => ({
  id,
  short_id: id.slice(0, 4),
  epic_id: epicId,
  title,
  description: "Story description",
  design: "",
  acceptance_criteria: [],
  issue_type: "task",
  status,
  priority: 2,
  labels: [],
  memory_refs: [],
  owner: null,
  created_at: new Date("2026-01-01").toISOString(),
  updated_at: new Date("2026-01-01").toISOString(),
  reopen_count: 0,
  continuation_count: 0,
});

const meta = {
  title: "Roadmap/EpicCard",
  component: EpicCard,
  parameters: { layout: "padded" },
} as const;

export default meta;

const epic = makeEpic("epic-1", "Storybook Epic");

export const Progress0Collapsed = {
  args: {
    epic,
    mockTasks: [
      makeTask("t1", epic.id, "Task 1", "open"),
      makeTask("t2", epic.id, "Task 2", "in_progress"),
    ],
    defaultExpanded: false,
  },
};

export const Progress50Expanded = {
  args: {
    epic: makeEpic("epic-2", "Half Done Epic"),
    mockTasks: [
      makeTask("t3", "epic-2", "Task 1", "closed"),
      makeTask("t4", "epic-2", "Task 2", "open"),
    ],
    defaultExpanded: true,
  },
};

export const Progress100ExpandedManyTasks = {
  args: {
    epic: makeEpic("epic-3", "Completed Epic"),
    mockTasks: [
      makeTask("t5", "epic-3", "Task 1", "closed"),
      makeTask("t6", "epic-3", "Task 2", "closed"),
      makeTask("t7", "epic-3", "Task 3", "closed"),
      makeTask("t8", "epic-3", "Task 4", "closed"),
      makeTask("t9", "epic-3", "Task 5", "closed"),
    ],
    defaultExpanded: true,
  },
};
