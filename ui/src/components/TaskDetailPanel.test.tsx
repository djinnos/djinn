import { describe, expect, it, vi } from "vitest";
import { TaskDetailPanel } from "./TaskDetailPanel";
import { render, screen } from "@/test/test-utils";
import { mockEpicA, mockTaskA, mockCiPassing, mockCiPending, mockCiFailing, mockCiUnknown, mockCiAdvisoryFailure } from "@/test/fixtures";

vi.mock("@/hooks/useTaskActions", () => ({
  useTaskActions: () => ({ busy: false, transition: vi.fn() }),
}));

vi.mock("@/hooks/useExecutionControl", () => ({
  useExecutionControl: () => ({ busy: false, killTask: vi.fn() }),
}));

vi.mock("@/api/users", async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>();
  return {
    ...actual,
    fetchUsers: vi.fn(async () => [
      {
        id: "user-alice",
        github_login: "alice",
        github_name: "Alice",
        github_avatar_url: null,
        is_member_of_org: true,
        is_admin: false,
      },
    ]),
  };
});

vi.mock("@/stores/useProjectStore", async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>();
  return {
    ...actual,
    useSelectedProject: () => ({ id: "p1", name: "Test", path: "/tmp/test" }),
  };
});

describe("TaskDetailPanel", () => {
  it("renders full metadata including AC list", async () => {
    const task = {
      ...mockTaskA,
      short_id: "tsk1",
      title: "Detailed task",
      description: "Task body markdown content",
      design: "Design section content",
      status: "in_progress",
      priority: 2,
      created_by_user_id: "user-alice",
      labels: ["frontend"],
      acceptance_criteria: [
        { criterion: "criterion met", met: true },
        { criterion: "criterion unmet", met: false },
      ],
    };

    render(<TaskDetailPanel task={task} epic={mockEpicA} open onClose={() => {}} />);

    expect(screen.getByText(task.title)).toBeInTheDocument();
    expect(screen.getByText(task.short_id)).toBeInTheDocument();
    expect(screen.getByText(/in flight — coding/i)).toBeInTheDocument();
    expect(screen.getByText(/p2/i)).toBeInTheDocument();
    expect(screen.getByText(/epic one/i)).toBeInTheDocument();
    expect(await screen.findByText(/alice/i)).toBeInTheDocument();
    expect(screen.getByText(task.description)).toBeInTheDocument();
    expect(screen.getByText(task.design)).toBeInTheDocument();
    expect(screen.getByText(task.acceptance_criteria[0].criterion)).toBeInTheDocument();
    expect(screen.getByText(task.acceptance_criteria[1].criterion)).toBeInTheDocument();

    const checkboxes = screen.getAllByRole("checkbox");
    expect(checkboxes).toHaveLength(2);
    expect(checkboxes[0]).toBeChecked();
    expect(checkboxes[1]).not.toBeChecked();
  });

  it("renders CI: passing status section when ci snapshot is passing", () => {
    const task = {
      ...mockTaskA,
      short_id: "ci-pass",
      title: "CI passing task",
      status: "in_progress",
      ci: mockCiPassing,
    };

    render(<TaskDetailPanel task={task} epic={mockEpicA} open onClose={() => {}} />);

    expect(screen.getByText(/CI Status/i)).toBeInTheDocument();
    expect(screen.getByText("Passing")).toBeInTheDocument();
    expect(screen.getByText("abc12345")).toBeInTheDocument();
    expect(screen.getByText(/#42/)).toBeInTheDocument();
  });

  it("renders CI: pending status as visible non-terminal state", () => {
    const task = {
      ...mockTaskA,
      short_id: "ci-pend",
      title: "CI pending task",
      status: "in_progress",
      ci: mockCiPending,
    };

    render(<TaskDetailPanel task={task} epic={mockEpicA} open onClose={() => {}} />);

    expect(screen.getByText(/CI Status/i)).toBeInTheDocument();
    expect(screen.getByText("Pending")).toBeInTheDocument();
  });

  it("renders awaiting CI derived gate state from structured API fields", () => {
    render(
      <TaskDetailPanel
        task={{ ...mockTaskA, title: "Awaiting CI task", ci: { ...mockCiPending, gate_state: "awaiting_ci" } }}
        epic={mockEpicA}
        open
        onClose={() => {}}
      />,
    );

    expect(screen.getByText("Awaiting CI")).toBeInTheDocument();
    expect(screen.getByText("Required checks pending")).toBeInTheDocument();
  });

  it("renders CI: failing status with blocking checks from structured fields", () => {
    const task = {
      ...mockTaskA,
      short_id: "ci-fail",
      title: "CI failing task",
      status: "in_progress",
      ci: mockCiFailing,
    };

    render(<TaskDetailPanel task={task} epic={mockEpicA} open onClose={() => {}} />);

    expect(screen.getByText(/CI Status/i)).toBeInTheDocument();
    expect(screen.getByText("Failing")).toBeInTheDocument();
    expect(screen.getByText("Quality Gate")).toBeInTheDocument();
    expect(screen.getByText("Server Tests")).toBeInTheDocument();
    expect(screen.getByText("Blocking checks:")).toBeInTheDocument();
    expect(screen.getByText("Required check failing: Quality Gate")).toBeInTheDocument();
    expect(screen.getByText("Blocked by failing required check: Quality Gate")).toBeInTheDocument();
    expect(screen.getByText("Repeat count:")).toBeInTheDocument();
    expect(screen.getByText("3")).toBeInTheDocument();
  });

  it("shows required red CI blocking reason even for closed presentation", () => {
    render(
      <TaskDetailPanel
        task={{ ...mockTaskA, short_id: "ci-closed", title: "Closed but red CI", status: "closed", ci: mockCiFailing }}
        epic={mockEpicA}
        open
        onClose={() => {}}
      />,
    );

    expect(screen.getByText("Failing")).toBeInTheDocument();
    expect(screen.getByText("Merge blocked reason:")).toBeInTheDocument();
    expect(screen.getByText("Blocked by failing required check: Quality Gate")).toBeInTheDocument();
  });

  it("does not render advisory/non-required failures as blocking when required CI is passing", () => {
    render(<TaskDetailPanel task={{ ...mockTaskA, title: "Advisory check failed", ci: mockCiAdvisoryFailure }} epic={mockEpicA} open onClose={() => {}} />);

    expect(screen.getByText("Passing")).toBeInTheDocument();
    expect(screen.queryByText("Blocking checks:")).not.toBeInTheDocument();
    expect(screen.queryByText("Merge blocked reason:")).not.toBeInTheDocument();
  });

  it("renders CI: unknown status as visible non-terminal state", () => {
    const task = {
      ...mockTaskA,
      short_id: "ci-unk",
      title: "CI unknown task",
      status: "open",
      ci: mockCiUnknown,
    };

    render(<TaskDetailPanel task={task} epic={mockEpicA} open onClose={() => {}} />);

    expect(screen.getByText(/CI Status/i)).toBeInTheDocument();
    expect(screen.getByText("Unknown")).toBeInTheDocument();
  });

  it("does not render CI section when ci field is absent", () => {
    const task = {
      ...mockTaskA,
      short_id: "no-ci",
      title: "No CI task",
      status: "open",
      ci: undefined,
    };

    render(<TaskDetailPanel task={task} epic={mockEpicA} open onClose={() => {}} />);

    expect(screen.queryByText(/CI Status/i)).not.toBeInTheDocument();
  });
});
