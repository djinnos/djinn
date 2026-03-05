import { RoadmapView } from "./RoadmapView";
import type { Epic, Task } from "@/types";

const makeEpic = (id: string, title: string, priority: Epic["priority"], createdAt: string): Epic => ({
  id,
  title,
  description: "Epic description",
  priority,
  status: "active",
  labels: [],
  owner: null,
  createdAt,
  updatedAt: createdAt,
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

const epics: Epic[] = [
  makeEpic("epic-a", "Foundation", "P0", "2026-01-03T00:00:00.000Z"),
  makeEpic("epic-b", "UX Polish", "P1", "2026-01-02T00:00:00.000Z"),
  makeEpic("epic-c", "Integrations", "P2", "2026-01-01T00:00:00.000Z"),
];

const tasks: Task[] = [
  makeTask("a1", "epic-a", "Set up core API", "completed"),
  makeTask("a2", "epic-a", "Define schema", "completed"),
  makeTask("b1", "epic-b", "Refine spacing", "in_progress"),
  makeTask("b2", "epic-b", "Tune colors", "pending"),
  makeTask("c1", "epic-c", "Connect webhooks", "blocked"),
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
