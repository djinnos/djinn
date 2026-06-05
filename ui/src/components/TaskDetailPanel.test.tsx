import { describe, expect, it, vi } from "vitest";
import { TaskDetailPanel } from "./TaskDetailPanel";
import { render, screen } from "@/test/test-utils";
import { mockEpicA, mockTaskA } from "@/test/fixtures";

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
});
