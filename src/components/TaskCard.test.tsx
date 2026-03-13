import { describe, expect, it } from "vitest";
import { TaskCard } from "./TaskCard";
import { render, screen } from "@/test/test-utils";
import { mockTaskA } from "@/test/fixtures";

describe("TaskCard", () => {
  it("renders title, short_id, status badge, priority, labels, and AC count", () => {
    const task = {
      ...mockTaskA,
      id: "task-card-1",
      short_id: "dt9t",
      status: "in_progress",
      priority: 1,
      title: "Implement task card tests",
      labels: ["frontend", "qa"],
      acceptance_criteria: [
        { criterion: "first", met: false },
        { criterion: "second", met: true },
      ],
      unresolved_blocker_count: 1,
    };

    render(<TaskCard task={task} />);

    expect(screen.getByText(task.title)).toBeInTheDocument();
    expect(screen.getByText(task.short_id)).toBeInTheDocument();
    expect(screen.getByLabelText(/pipeline: coding/i)).toBeInTheDocument();
    expect(screen.getByLabelText(`Priority P${task.priority}`)).toBeInTheDocument();
    expect(screen.getByText("1/2")).toBeInTheDocument();
    expect(screen.getByText(/frontend/i)).toBeInTheDocument();
  });
});
