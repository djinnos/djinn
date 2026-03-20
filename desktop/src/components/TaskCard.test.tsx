import { describe, expect, it } from "vitest";
import { TaskCard } from "./TaskCard";
import { render, screen } from "@/test/test-utils";
import { mockTaskA } from "@/test/fixtures";
import { verificationStore } from "@/stores/verificationStore";

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

  it("renders setup and verifying badge text with active step names", () => {
    const setupTask = {
      ...mockTaskA,
      id: "task-setup-step",
      short_id: "s1",
      status: "in_progress",
      title: "Setup task",
    };

    const verifyingTask = {
      ...mockTaskA,
      id: "task-verify-step",
      short_id: "v1",
      status: "verifying",
      title: "Verifying task",
    };

    verificationStore.getState().clearLifecycleSteps(setupTask.id);
    verificationStore.getState().clearRun("run-setup");
    verificationStore.getState().clearLifecycleSteps(verifyingTask.id);
    verificationStore.getState().clearRun("run-verify");

    verificationStore.getState().addLifecycleStep(setupTask.id, {
      step: "cargo build",
      detail: "",
      status: "running",
      timestamp: new Date().toISOString(),
    });

    verificationStore.getState().addStep("run-setup", {
      index: 0,
      name: "cargo test",
      phase: "verification",
      status: "running",
    }, {
      projectId: "project-a",
      taskId: setupTask.id,
    });

    verificationStore.getState().addStep("run-verify", {
      index: 0,
      name: "cargo test",
      phase: "verification",
      status: "running",
    }, {
      projectId: "project-a",
      taskId: verifyingTask.id,
    });

    const { rerender } = render(<TaskCard task={setupTask} />);
    // During setup, only the "setting up" badge is shown; details are in the tooltip
    const setupBadge = screen.getByText("setting up");
    expect(setupBadge).toBeInTheDocument();
    expect(setupBadge).toHaveAttribute("title", expect.stringContaining("cargo build"));

    rerender(<TaskCard task={verifyingTask} />);
    expect(screen.getByText("verifying: cargo test")).toBeInTheDocument();

    verificationStore.getState().clearLifecycleSteps(setupTask.id);
    verificationStore.getState().clearRun("run-setup");
    verificationStore.getState().clearLifecycleSteps(verifyingTask.id);
    verificationStore.getState().clearRun("run-verify");
  });
});
