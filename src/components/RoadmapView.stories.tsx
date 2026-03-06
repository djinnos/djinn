import { RoadmapView } from "./RoadmapView";
import type { Epic, Task } from "@/api/types";

const makeEpic = (id: string, title: string, createdAt: string): Epic => ({
  id,
  short_id: id.slice(0, 4),
  title,
  description: "Epic description",
  emoji: "🚀",
  color: "#3B82F6",
  status: "active",
  owner: null,
  created_at: createdAt,
  updated_at: createdAt,
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

const epics: Epic[] = [
  makeEpic("epic-a", "Foundation", "2026-01-03T00:00:00.000Z"),
  makeEpic("epic-b", "UX Polish", "2026-01-02T00:00:00.000Z"),
  makeEpic("epic-c", "Integrations", "2026-01-01T00:00:00.000Z"),
];

const tasks: Task[] = [
  makeTask("a1", "epic-a", "Set up core API", "closed"),
  makeTask("a2", "epic-a", "Define schema", "closed"),
  makeTask("b1", "epic-b", "Refine spacing", "in_progress"),
  makeTask("b2", "epic-b", "Tune colors", "open"),
  makeTask("c1", "epic-c", "Connect webhooks", "open"),
];

const meta = {
  title: "Roadmap/RoadmapView",
  component: RoadmapView,
  parameters: { layout: "fullscreen" },
} as const;

export default meta;

export const MultipleEpics = {
  args: {
    mockEpics: epics,
    mockTasks: tasks,
  },
};
