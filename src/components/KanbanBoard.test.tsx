import { describe, it, expect } from "vitest";
import { render, screen, userEvent, waitFor, within } from "@/test/test-utils";
import { KanbanBoard } from "@/components/KanbanBoard";
import type { Epic, Task } from "@/api/types";

const epicA: Epic = {
  id: "epic-a",
  title: "Epic Alpha",
  description: "",
  status: "open",
  owner: null,
  priority: 1,
  issue_type: "epic",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
};

const epicB: Epic = {
  id: "epic-b",
  title: "Epic Beta",
  description: "",
  status: "open",
  owner: null,
  priority: 2,
  issue_type: "epic",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
};

const makeTask = (overrides: Partial<Task> & Pick<Task, "id" | "title" | "status">): Task => ({
  id: overrides.id,
  title: overrides.title,
  status: overrides.status,
  description: "",
  owner: null,
  priority: 1,
  issue_type: "task",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
  epic_id: epicA.id,
  labels: [],
  ...overrides,
});

describe("KanbanBoard", () => {
  it("renders status columns with tasks in correct columns and header counts", () => {
    const tasks: Task[] = [
      makeTask({ id: "t-backlog", title: "Backlog task", status: "backlog", epic_id: epicA.id }),
      makeTask({ id: "t-open", title: "Open task", status: "open", epic_id: epicA.id }),
      makeTask({ id: "t-flight", title: "Flight task", status: "in_progress", epic_id: epicA.id }),
      makeTask({ id: "t-done", title: "Done task", status: "closed", epic_id: epicA.id }),
    ];

    render(
      <KanbanBoard
        tasks={tasks}
        epics={new Map([
          [epicA.id, epicA],
          [epicB.id, epicB],
        ])}
        disableSearchParamSync
      />
    );

    const backlogCol = screen.getByText("Backlog").closest(".flex.flex-col");
    const openCol = screen.getByText("Open").closest(".flex.flex-col");
    const inFlightCol = screen.getByText("In Flight").closest(".flex.flex-col");
    const doneCol = screen.getByText("Done").closest(".flex.flex-col");

    expect(backlogCol).toHaveTextContent("1");
    expect(openCol).toHaveTextContent("1");
    expect(inFlightCol).toHaveTextContent("1");
    expect(doneCol).toHaveTextContent("1");

    expect(within(backlogCol!.parentElement as HTMLElement).getByText("Backlog task")).toBeInTheDocument();
    expect(within(openCol!.parentElement as HTMLElement).getByText("Open task")).toBeInTheDocument();
    expect(within(inFlightCol!.parentElement as HTMLElement).getByText("Flight task")).toBeInTheDocument();
    expect(within(doneCol!.parentElement as HTMLElement).getByText("Done task")).toBeInTheDocument();
  });

  it("filters by epic and text search", async () => {
    const user = userEvent.setup();
    const tasks: Task[] = [
      makeTask({ id: "t-a-1", title: "Alpha target task", status: "open", epic_id: epicA.id, priority: 1 }),
      makeTask({ id: "t-a-2", title: "Alpha other", status: "open", epic_id: epicA.id, priority: 1 }),
      makeTask({ id: "t-b-1", title: "Beta target task", status: "open", epic_id: epicB.id, priority: 2 }),
    ];

    render(
      <KanbanBoard
        tasks={tasks}
        epics={new Map([
          [epicA.id, epicA],
          [epicB.id, epicB],
        ])}
        disableSearchParamSync
      />
    );

    await user.click(screen.getByPlaceholderText("All epics"));
    await user.click(screen.getByRole("option", { name: /Epic Alpha/i }));
    await user.keyboard("{Escape}");

    expect(screen.getByText("Alpha target task")).toBeInTheDocument();
    expect(screen.getByText("Alpha other")).toBeInTheDocument();
    expect(screen.queryByText("Beta target task")).not.toBeInTheDocument();

    await user.type(screen.getByPlaceholderText("Search tasks..."), "target");

    await waitFor(() => {
      expect(screen.getByText("Alpha target task")).toBeInTheDocument();
      expect(screen.queryByText("Alpha other")).not.toBeInTheDocument();
      expect(screen.queryByText("Beta target task")).not.toBeInTheDocument();
    });
  });

  it("shows empty board state when no tasks", () => {
    render(<KanbanBoard tasks={[]} epics={new Map()} disableSearchParamSync />);

    expect(screen.getAllByText("No tasks")).toHaveLength(4);
  });
});
