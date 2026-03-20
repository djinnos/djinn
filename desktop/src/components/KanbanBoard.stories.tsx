import { MemoryRouter } from "react-router-dom";
import { KanbanBoard } from "@/components/KanbanBoard";
import type { Epic, Task } from "@/api/types";

type BoardFixture = {
  epics: Epic[];
  tasks: Task[];
  initialPath?: string;
  initialCollapsedEpics?: string[];
};

const emptyFixture: BoardFixture = {
  epics: [],
  tasks: [],
};

const makeEpic = (id: string, title: string, emoji: string, owner: string): Epic => ({
  id,
  short_id: id.slice(0, 4),
  title,
  description: "",
  emoji,
  color: "#3B82F6",
  status: "active",
  owner,
  created_at: "2026-03-01T10:00:00.000Z",
  updated_at: "2026-03-01T10:00:00.000Z",
});

const epicsFixture: Epic[] = [
  makeEpic("epic-foundation", "Platform Foundation", "🚀", "Alex"),
  makeEpic("epic-ux", "UX Polish", "🎨", "Mina"),
  makeEpic("epic-auth", "Authentication", "🔐", "Priya"),
];

const makeTask = (
  id: string,
  title: string,
  status: string,
  priority: number,
  owner: string,
  epicId: string | undefined,
  labels: string[],
  ts: string,
  overrides?: Partial<Task>,
): Task => ({
  id,
  short_id: id.slice(0, 4),
  title,
  description: "",
  design: "",
  acceptance_criteria: [],
  issue_type: "task",
  status,
  priority,
  owner,
  epic_id: epicId,
  labels,
  memory_refs: [],
  created_at: ts,
  updated_at: ts,
  reopen_count: 0,
  continuation_count: 0,
  unresolved_blocker_count: 0,
  ...overrides,
});

const tasksFixture: Task[] = [
  // Open
  makeTask("t-1", "Scaffold auth hooks", "open", 1, "Alex", "epic-foundation", ["auth"], "2026-03-01T11:00:00.000Z"),
  makeTask("t-13", "Setup config loader", "open", 1, "Alex", "epic-foundation", [], "2026-03-01T11:05:00.000Z"),
  makeTask("t-14", "Token refresh logic", "open", 0, "Priya", "epic-auth", [], "2026-03-01T11:10:00.000Z", { reopen_count: 2 }),
  makeTask("t-15", "Keyboard shortcut manager", "open", 1, "Mina", "epic-ux", [], "2026-03-01T11:15:00.000Z", { unresolved_blocker_count: 2 }),
  makeTask("t-5", "Backfill migration docs", "open", 3, "Jordan", undefined, ["docs"], "2026-03-01T11:40:00.000Z"),

  // In Flight — coding
  makeTask("t-2", "Set up observability alerts", "in_progress", 0, "Priya", "epic-foundation", ["infra"], "2026-03-01T11:10:00.000Z", {
    active_session: { model_id: "claude-3.5-sonnet", started_at: new Date(Date.now() - 720_000).toISOString() },
    duration_seconds: 300,
  }),

  // In Flight — verifying
  makeTask("t-6", "Run integration test suite", "verifying", 1, "Priya", "epic-foundation", ["ci"], "2026-03-01T11:15:00.000Z", {
    duration_seconds: 480,
  }),

  // In Flight — review
  makeTask("t-3", "Refine empty states", "needs_task_review", 2, "Mina", "epic-ux", ["ui"], "2026-03-01T11:20:00.000Z", {
    duration_seconds: 1860,
  }),

  // In Flight — Lead intervention (red tint)
  makeTask("t-17", "Credential validation flow", "in_pm_intervention", 0, "Priya", "epic-auth", [], "2026-03-01T11:30:00.000Z", {
    duration_seconds: 360,
  }),

  // Done
  makeTask("t-4", "Keyboard navigation pass", "closed", 1, "Alex", "epic-ux", ["accessibility"], "2026-03-01T11:30:00.000Z", {
    duration_seconds: 1380,
  }),
  makeTask("t-7", "SSE initial connect", "closed", 1, "Priya", "epic-foundation", [], "2026-03-01T11:35:00.000Z", {
    duration_seconds: 300,
  }),
];

const meta = {
  title: "Kanban/KanbanBoard",
  component: KanbanBoard,
  parameters: {
    layout: "fullscreen",
  },
  decorators: [
    (_StoryFn: unknown, context: { args: { fixture?: BoardFixture } }) => {
      const fixture = context.args.fixture ?? emptyFixture;
      const path = fixture.initialPath ?? "/";

      return (
        <MemoryRouter initialEntries={[path]}>
          <div className="h-screen p-4">
            <KanbanBoard
              tasks={fixture.tasks}
              epics={new Map(fixture.epics.map((epic) => [epic.id, epic]))}
              initialCollapsedEpics={fixture.initialCollapsedEpics}
            />
          </div>
        </MemoryRouter>
      );
    },
  ],
};

export default meta;

export const EmptyBoard = {
  args: {
    fixture: emptyFixture,
  },
};

export const PopulatedAcrossColumns = {
  args: {
    fixture: {
      epics: epicsFixture,
      tasks: tasksFixture,
    },
  },
};

export const CollapsedEpicGroups = {
  args: {
    fixture: {
      epics: epicsFixture,
      tasks: tasksFixture,
      initialCollapsedEpics: ["open:epic-foundation", "done:epic-ux"],
    },
  },
};

export const FilteredView = {
  args: {
    fixture: {
      epics: epicsFixture,
      tasks: tasksFixture,
      initialPath: "/?owner=Alex&priority=P1",
    },
  },
};
