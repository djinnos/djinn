import { beforeEach, describe, it, expect, vi } from "vitest";
import { render, screen, userEvent, waitFor, within } from "@/test/test-utils";
import { KanbanBoard } from "@/components/KanbanBoard";
import { mergedColumnStore } from "@/stores/mergedColumnStore";
import { loadMoreClosedTasks } from "@/api/server";
import type { Epic, Task } from "@/api/types";

vi.mock("@/electron/commands", () => ({
  checkGitRemote: vi.fn(),
  setupGitRemote: vi.fn(),
}));

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: vi.fn(),
}));

vi.mock("@/api/server", () => ({
  loadMoreClosedTasks: vi.fn().mockResolvedValue([]),
  searchTasksAcrossProjects: vi.fn().mockResolvedValue([]),
}));

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

const draftEpic: Epic = {
  id: "epic-draft",
  title: "Epic Draft",
  description: "",
  status: "open",
  owner: null,
  priority: 2,
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

const makeTask = (
  overrides: Partial<Task> & Pick<Task, "id" | "title" | "status">,
): Task => ({
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
  beforeEach(() => {
    vi.clearAllMocks();
    // The Merged-column pagination state is a global store; reset it so tests
    // that don't opt into "Load more" see no affordance.
    mergedColumnStore.getState().reset();
  });

  it("renders status columns with tasks in correct columns and header counts", () => {
    const tasks: Task[] = [
      makeTask({
        id: "t-open",
        title: "Open task",
        status: "open",
        epic_id: epicA.id,
      }),
      makeTask({
        id: "t-flight",
        title: "Flight task",
        status: "in_progress",
        epic_id: epicA.id,
      }),
      makeTask({
        id: "t-done",
        title: "Done task",
        status: "closed",
        epic_id: epicA.id,
        merge_commit_sha: "abc123merged",
      }),
    ];

    render(
      <KanbanBoard
        tasks={tasks}
        epics={
          new Map([
            [epicA.id, epicA],
            [epicB.id, epicB],
          ])
        }
      />,
    );

    const openCol = screen.getByText("Open").closest(".flex.flex-col");
    const inFlightCol = screen
      .getByText("In Progress")
      .closest(".flex.flex-col");
    const doneCol = screen.getByText("Merged").closest(".flex.flex-col");

    expect(openCol).toHaveTextContent("1");
    expect(inFlightCol).toHaveTextContent("1");
    expect(doneCol).toHaveTextContent("1");

    expect(
      within(openCol!.parentElement as HTMLElement).getByText("Open task"),
    ).toBeInTheDocument();
    expect(
      within(inFlightCol!.parentElement as HTMLElement).getByText(
        "Flight task",
      ),
    ).toBeInTheDocument();
    expect(
      within(doneCol!.parentElement as HTMLElement).getByText("Done task"),
    ).toBeInTheDocument();
  });

  it("applies the text search from the shared filter URL param", async () => {
    // The search box now lives in the shared BoardFilterHeader, which writes
    // the query to the `q` URL param; the board reads it back. Drive filtering
    // by seeding that param directly.
    const tasks: Task[] = [
      makeTask({
        id: "t-a-1",
        title: "Alpha target task",
        status: "open",
        epic_id: epicA.id,
        priority: 1,
      }),
      makeTask({
        id: "t-a-2",
        title: "Alpha other",
        status: "open",
        epic_id: epicA.id,
        priority: 1,
      }),
      makeTask({
        id: "t-b-1",
        title: "Beta target task",
        status: "open",
        epic_id: epicB.id,
        priority: 2,
      }),
    ];

    render(
      <KanbanBoard
        tasks={tasks}
        epics={
          new Map([
            [epicA.id, epicA],
            [epicB.id, epicB],
          ])
        }
      />,
      { wrapperOptions: { routerProps: { initialEntries: ["/tasks?q=target"] } } },
    );

    await waitFor(() => {
      expect(screen.getByText("Alpha target task")).toBeInTheDocument();
      expect(screen.getByText("Beta target task")).toBeInTheDocument();
      expect(screen.queryByText("Alpha other")).not.toBeInTheDocument();
    });
  });

  it("only shows closed tasks with landed merge SHAs in the merged column", () => {
    const tasks: Task[] = [
      makeTask({
        id: "t-merged-pr",
        title: "Merged PR task",
        status: "closed",
        issue_type: "task",
        epic_id: epicA.id,
        pr_url: "https://github.com/example/repo/pull/42",
        merge_commit_sha: "abc123merged",
      }),
      makeTask({
        id: "t-direct-push",
        title: "Direct push merged task",
        status: "closed",
        issue_type: "task",
        epic_id: epicA.id,
        pr_url: null,
        merge_commit_sha: "def456direct",
      }),
      makeTask({
        id: "t-closed-no-merge",
        title: "Closed without merge",
        status: "closed",
        issue_type: "task",
        epic_id: epicA.id,
      }),
      makeTask({
        id: "t-closed-review",
        title: "Closed review without merge",
        status: "closed",
        issue_type: "review",
        epic_id: epicA.id,
      }),
      makeTask({
        id: "t-closed-planning",
        title: "Closed planning without merge",
        status: "closed",
        issue_type: "planning",
        epic_id: epicA.id,
      }),
    ];

    render(
      <KanbanBoard
        tasks={tasks}
        epics={new Map([[epicA.id, epicA]])}
      />,
    );

    const doneCol = screen.getByText("Merged").closest(".flex.flex-col");
    expect(doneCol).toHaveTextContent("2");
    expect(screen.getByText("Merged PR task")).toBeInTheDocument();
    expect(screen.getByText("Direct push merged task")).toBeInTheDocument();
    expect(screen.queryByText("Closed without merge")).not.toBeInTheDocument();
    expect(
      screen.queryByText("Closed review without merge"),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByText("Closed planning without merge"),
    ).not.toBeInTheDocument();
  });

  it("treats legacy PR-completed tasks (no merge SHA) as merged, but not force-closed ones", () => {
    const tasks: Task[] = [
      makeTask({
        id: "t-legacy-merged",
        title: "Legacy merged task",
        status: "closed",
        issue_type: "task",
        epic_id: epicA.id,
        pr_url: "https://github.com/example/repo/pull/7",
        close_reason: "completed",
        // No merge_commit_sha — closed before the SHA was persisted.
      }),
      makeTask({
        id: "t-force-closed",
        title: "Force closed unmerged task",
        status: "closed",
        issue_type: "task",
        epic_id: epicA.id,
        pr_url: "https://github.com/example/repo/pull/8",
        close_reason: "force_closed",
      }),
    ];

    render(
      <KanbanBoard tasks={tasks} epics={new Map([[epicA.id, epicA]])} />,
    );

    const doneCol = screen.getByText("Merged").closest(".flex.flex-col");
    expect(doneCol).toHaveTextContent("1");
    expect(screen.getByText("Legacy merged task")).toBeInTheDocument();
    expect(
      screen.queryByText("Force closed unmerged task"),
    ).not.toBeInTheDocument();
  });

  it("shows empty board state when no tasks", () => {
    render(<KanbanBoard tasks={[]} epics={new Map()} />);

    expect(screen.getAllByText("No tasks")).toHaveLength(4);
  });

  it("suffixes the Merged count with '+' and shows Load more when more closed pages exist", async () => {
    // Simulate the first closed page having been loaded with more remaining.
    mergedColumnStore.getState().setProjectPage({
      slug: "owner/repo",
      projectId: "p1",
      loaded: 200,
      hasMore: true,
      totalClosed: 1231,
    });

    const tasks: Task[] = [
      makeTask({
        id: "t-done",
        title: "Merged task",
        status: "closed",
        epic_id: epicA.id,
        merge_commit_sha: "abc123merged",
      }),
    ];

    render(<KanbanBoard tasks={tasks} epics={new Map([[epicA.id, epicA]])} />);

    // One merged task loaded, but the server has more → "1+".
    const doneCol = screen.getByText("Merged").closest(".flex.flex-col");
    expect(doneCol).toHaveTextContent("1+");

    const loadMore = screen.getByRole("button", { name: "Load more" });
    await userEvent.click(loadMore);
    expect(loadMoreClosedTasks).toHaveBeenCalledTimes(1);
  });

  it("does not show Load more or a '+' suffix when all closed pages are loaded", () => {
    mergedColumnStore.getState().setProjectPage({
      slug: "owner/repo",
      projectId: "p1",
      loaded: 1,
      hasMore: false,
      totalClosed: 1,
    });

    const tasks: Task[] = [
      makeTask({
        id: "t-done",
        title: "Merged task",
        status: "closed",
        epic_id: epicA.id,
        merge_commit_sha: "abc123merged",
      }),
    ];

    render(<KanbanBoard tasks={tasks} epics={new Map([[epicA.id, epicA]])} />);

    const doneCol = screen.getByText("Merged").closest(".flex.flex-col");
    expect(doneCol).toHaveTextContent("1");
    expect(doneCol).not.toHaveTextContent("1+");
    expect(
      screen.queryByRole("button", { name: "Load more" }),
    ).not.toBeInTheDocument();
  });

  it("shows empty open epics in the Open column when they have no tasks", () => {
    render(
      <KanbanBoard
        tasks={[]}
        epics={
          new Map([
            [epicA.id, epicA],
            [draftEpic.id, draftEpic],
          ])
        }
      />,
    );

    const openCol = screen.getByText("Open").closest(".flex.flex-col")
      ?.parentElement as HTMLElement;

    expect(within(openCol).getByText("Epic Alpha")).toBeInTheDocument();
    expect(within(openCol).getByText("Epic Draft")).toBeInTheDocument();
    expect(within(openCol).getAllByText("No tasks yet")).toHaveLength(2);
  });
});
