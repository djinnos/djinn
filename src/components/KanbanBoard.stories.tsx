import { MemoryRouter } from "react-router-dom";
import { KanbanBoard } from "@/components/KanbanBoard";
import type { Epic, Task } from "@/types";

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

const epicsFixture: Epic[] = [
  {
    id: "epic-foundation",
    title: "Platform Foundation",
    description: "Core infra and shared systems",
    status: "active",
    priority: "P1",
    labels: ["platform"],
    owner: "Alex",
    createdAt: "2026-03-01T10:00:00.000Z",
    updatedAt: "2026-03-01T10:00:00.000Z",
  },
  {
    id: "epic-ux",
    title: "UX Polish",
    description: "Usability and visual improvements",
    status: "active",
    priority: "P2",
    labels: ["ux"],
    owner: "Mina",
    createdAt: "2026-03-01T10:00:00.000Z",
    updatedAt: "2026-03-01T10:00:00.000Z",
  },
];

const tasksFixture: Task[] = [
  {
    id: "t-1",
    title: "Scaffold auth hooks",
    description: "",
    design: "",
    acceptanceCriteria: [],
    activity: [],
    status: "pending",
    priority: "P1",
    owner: "Alex",
    epicId: "epic-foundation",
    labels: ["auth"],
    createdAt: "2026-03-01T11:00:00.000Z",
    updatedAt: "2026-03-01T11:00:00.000Z",
  },
  {
    id: "t-2",
    title: "Set up observability alerts",
    description: "",
    design: "",
    acceptanceCriteria: [],
    activity: [],
    status: "in_progress",
    priority: "P0",
    owner: "Priya",
    epicId: "epic-foundation",
    labels: ["infra"],
    createdAt: "2026-03-01T11:10:00.000Z",
    updatedAt: "2026-03-01T11:10:00.000Z",
  },
  {
    id: "t-3",
    title: "Refine empty states",
    description: "",
    design: "",
    acceptanceCriteria: [],
    activity: [],
    status: "blocked",
    priority: "P2",
    owner: "Mina",
    epicId: "epic-ux",
    labels: ["ui"],
    createdAt: "2026-03-01T11:20:00.000Z",
    updatedAt: "2026-03-01T11:20:00.000Z",
  },
  {
    id: "t-4",
    title: "Keyboard navigation pass",
    description: "",
    design: "",
    acceptanceCriteria: [],
    activity: [],
    status: "completed",
    priority: "P1",
    owner: "Alex",
    epicId: "epic-ux",
    labels: ["accessibility"],
    createdAt: "2026-03-01T11:30:00.000Z",
    updatedAt: "2026-03-01T11:30:00.000Z",
  },
  {
    id: "t-5",
    title: "Backfill migration docs",
    description: "",
    design: "",
    acceptanceCriteria: [],
    activity: [],
    status: "pending",
    priority: "P3",
    owner: "Jordan",
    epicId: null,
    labels: ["docs"],
    createdAt: "2026-03-01T11:40:00.000Z",
    updatedAt: "2026-03-01T11:40:00.000Z",
  },
];

const meta = {
  title: "Planning/KanbanBoard",
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

export const EpicGrouping = {
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
      initialCollapsedEpics: ["pending:epic-foundation", "completed:epic-ux"],
    },
  },
};

export const FilteredView = {
  args: {
    fixture: {
      epics: epicsFixture,
      tasks: tasksFixture,
      initialPath: "/?owner=Alex&priority=P1&q=Keyboard",
    },
  },
};
