import { describe, expect, it } from "vitest";
import { TaskCard } from "./TaskCard";
import { render, screen } from "@/test/test-utils";
import { mockTaskA, mockCiPassing, mockCiPending, mockCiFailing, mockCiUnknown, mockCiAdvisoryFailure } from "@/test/fixtures";

describe("TaskCard", () => {
  it("renders title, short_id, status badge, priority, and AC count (labels stay off the card)", () => {
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
      active_session: {
        session_id: "run-starting-1",
        agent_type: "",
        model_id: "",
        started_at: new Date().toISOString(),
        status: "starting",
      },
    };

    render(<TaskCard task={task} />);

    expect(screen.getByText(task.title)).toBeInTheDocument();
    expect(screen.getByText(task.short_id)).toBeInTheDocument();
    expect(screen.getByText("starting")).toBeInTheDocument();
    expect(screen.getByLabelText(`Priority P${task.priority}`)).toBeInTheDocument();
    expect(screen.getByText("1/2")).toBeInTheDocument();
    // Label chips were dropped from the card to keep rows scannable.
    expect(screen.queryByText(/frontend/i)).not.toBeInTheDocument();
  });

  it("renders the real 'starting' status (not a derived 'setting up') for a dispatched, pre-session run", () => {
    const task = {
      ...mockTaskA,
      id: "task-setup-step",
      short_id: "s1",
      status: "in_progress",
      title: "Setup task",
      active_session: {
        session_id: "run-starting-2",
        agent_type: "",
        model_id: "",
        started_at: new Date().toISOString(),
        status: "starting",
      },
    };

    render(<TaskCard task={task} />);

    expect(screen.getByText("starting")).toBeInTheDocument();
    expect(screen.queryByText("setting up")).not.toBeInTheDocument();
  });

  it("never derives a 'setting up' pseudo-status from a missing session", () => {
    const task = {
      ...mockTaskA,
      id: "task-no-session",
      short_id: "s2",
      status: "in_progress",
      title: "In progress, no session yet",
      active_session: undefined,
    };

    render(<TaskCard task={task} />);

    expect(screen.queryByText("setting up")).not.toBeInTheDocument();
    expect(screen.queryByText("starting")).not.toBeInTheDocument();
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

  // ── m116 forward-compatibility ────────────────────────────────────────────
  //
  // The backend CiGateSnapshot now includes additive nullable reconciliation
  // fields (mirror_head_sha, github_head_sha, heads_diverged,
  // head_observation_error) that the UI's TypeScript interface does not
  // declare.  At runtime these arrive as extra JSON keys that are never
  // accessed.  This test simulates the real wire shape (JSON.parse of a
  // backend payload with the extra keys) and confirms the UI renders
  // existing fields correctly — the additive fields do not break consumers
  // that only read head_sha / status / etc.
  it("renders correctly when CI payload carries additive m116 reconciliation fields", () => {
    const ciWithM116Fields = JSON.parse(
      JSON.stringify({
        ...mockCiPassing,
        mirror_head_sha: "mirror1234567890abcdef",
        github_head_sha: "github9876543210fedcba",
        heads_diverged: true,
        head_observation_error: "push failed",
      }),
    );

    const task = {
      ...mockTaskA,
      id: "task-ci-m116-fields",
      short_id: "m116",
      ci: ciWithM116Fields,
    };

    render(<TaskCard task={task} />);

    const badge = screen.getByTestId("taskcard-ci-badge");
    expect(badge).toHaveTextContent("CI: passing");
  });
});
