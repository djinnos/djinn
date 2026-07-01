import { describe, expect, it } from "vitest";
import { TaskCard } from "./TaskCard";
import { render, screen } from "@/test/test-utils";
import { mockTaskA, mockCiPassing, mockCiPending, mockCiFailing, mockCiUnknown, mockCiAdvisoryFailure } from "@/test/fixtures";

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
    expect(screen.getByText("setting up")).toBeInTheDocument();
    expect(screen.getByLabelText(`Priority P${task.priority}`)).toBeInTheDocument();
    expect(screen.getByText("1/2")).toBeInTheDocument();
    expect(screen.getByText(/frontend/i)).toBeInTheDocument();
  });

  it("renders setting up badge for in-progress tasks without an active session", () => {
    const task = {
      ...mockTaskA,
      id: "task-setup-step",
      short_id: "s1",
      status: "in_progress",
      title: "Setup task",
    };

    render(<TaskCard task={task} />);

    expect(screen.getByText("setting up")).toBeInTheDocument();
  });

  it("renders CI: passing badge when ci status is passing", () => {
    const task = {
      ...mockTaskA,
      id: "task-ci-passing",
      short_id: "cp1",
      ci: mockCiPassing,
    };

    render(<TaskCard task={task} />);

    const badge = screen.getByTestId("taskcard-ci-badge");
    expect(badge).toHaveTextContent("CI: passing");
  });

  it("renders CI: pending badge with animation when ci status is pending", () => {
    const task = {
      ...mockTaskA,
      id: "task-ci-pending",
      short_id: "cp2",
      ci: mockCiPending,
    };

    render(<TaskCard task={task} />);

    const badge = screen.getByTestId("taskcard-ci-badge");
    expect(badge).toHaveTextContent("CI: pending");
  });

  it("renders derived awaiting_ci gate state from structured API fields", () => {
    render(
      <TaskCard
        task={{
          ...mockTaskA,
          id: "task-ci-awaiting",
          short_id: "caw",
          ci: { ...mockCiPending, gate_state: "awaiting_ci" },
        }}
      />,
    );

    expect(screen.getByTestId("taskcard-ci-badge")).toHaveTextContent("CI: awaiting_ci");
  });

  it("renders CI: failing badge with blocking check names from structured fields", () => {
    const task = {
      ...mockTaskA,
      id: "task-ci-failing",
      short_id: "cf1",
      ci: mockCiFailing,
    };

    render(<TaskCard task={task} />);

    const badge = screen.getByTestId("taskcard-ci-badge");
    expect(badge).toHaveTextContent("CI: failing");
    expect(badge).toHaveTextContent("Quality Gate");
    expect(badge).toHaveTextContent("+1");
    // title attribute shows blocking details
    expect(badge).toHaveAttribute("title", expect.stringContaining("Blocked by failing required check: Quality Gate"));
  });

  it("renders closed tasks with required red CI as still blocked by the structured CI reason", () => {
    render(<TaskCard task={{ ...mockTaskA, id: "task-closed-red-ci", status: "closed", ci: mockCiFailing }} />);

    const badge = screen.getByTestId("taskcard-ci-badge");
    expect(badge).toHaveTextContent("CI: failing");
    expect(badge).toHaveAttribute("title", expect.stringContaining("Quality Gate"));
  });

  it("renders advisory/non-required failures as non-blocking when required CI is passing", () => {
    render(<TaskCard task={{ ...mockTaskA, id: "task-advisory-ci", ci: mockCiAdvisoryFailure }} />);

    const badge = screen.getByTestId("taskcard-ci-badge");
    expect(badge).toHaveTextContent("CI: passing");
    expect(badge).not.toHaveAttribute("title");
  });

  it("renders CI: unknown badge when ci status is unknown", () => {
    const task = {
      ...mockTaskA,
      id: "task-ci-unknown",
      short_id: "cu1",
      ci: mockCiUnknown,
    };

    render(<TaskCard task={task} />);

    const badge = screen.getByTestId("taskcard-ci-badge");
    expect(badge).toHaveTextContent("CI: unknown");
  });

  it("does not render CI badge when ci field is absent", () => {
    const task = {
      ...mockTaskA,
      id: "task-no-ci",
      short_id: "nc1",
      ci: undefined,
    };

    render(<TaskCard task={task} />);

    expect(screen.queryByTestId("taskcard-ci-badge")).not.toBeInTheDocument();
  });
});
