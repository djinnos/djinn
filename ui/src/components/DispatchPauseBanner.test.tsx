import { act, render, screen } from "@/test/test-utils";
import { beforeEach, describe, expect, it } from "vitest";
import { DispatchPauseBanner } from "./DispatchPauseBanner";
import {
  dispatchPauseStore,
  type DispatchPauseEntry,
} from "@/stores/dispatchPauseStore";
import { ALL_PROJECTS, projectStore } from "@/stores/projectStore";

const pause = (
  scope: DispatchPauseEntry["scope"],
  targetId: string | null,
  reason: string,
  pausedBy = "ops-admin",
): DispatchPauseEntry => ({
  scope,
  target_id: targetId,
  paused_by: pausedBy,
  paused_at: "2026-01-01T12:00:00Z",
  reason,
});

describe("DispatchPauseBanner", () => {
  beforeEach(() => {
    dispatchPauseStore.getState().clearAll();
    projectStore.setState({
      selectedProjectId: "project-a",
      projects: [
        { id: "project-a", name: "Project A", path: "/tmp/a" },
        { id: "project-b", name: "Project B", path: "/tmp/b" },
      ],
    });
  });

  it("renders nothing when no pause applies", () => {
    const { container } = render(
      <DispatchPauseBanner
        entries={[]}
        selectedProjectId="project-a"
        currentUserId="user-1"
      />,
    );

    expect(container).toBeEmptyDOMElement();
  });

  it("shows a global pause for any selected project context", () => {
    render(
      <DispatchPauseBanner
        entries={[pause("global", null, "Global maintenance", "admin-1")]}
        selectedProjectId="unrelated-project"
        currentUserId="user-1"
      />,
    );

    expect(screen.getByRole("status", { name: "Dispatch paused" })).toBeInTheDocument();
    expect(screen.getByText("Global dispatch pause")).toBeInTheDocument();
    expect(screen.getByText("Reason: Global maintenance")).toBeInTheDocument();
    expect(screen.getByText("Paused by admin-1")).toBeInTheDocument();
  });

  it("filters project pauses to matching selected and all-project contexts", () => {
    const entries = [pause("project", "project-a", "Project A maintenance")];
    const { rerender } = render(
      <DispatchPauseBanner
        entries={entries}
        selectedProjectId="project-b"
        currentUserId="user-1"
        allProjectIds={["project-a", "project-b"]}
      />,
    );

    expect(screen.queryByText("Project A maintenance")).not.toBeInTheDocument();

    rerender(
      <DispatchPauseBanner
        entries={entries}
        selectedProjectId="project-a"
        currentUserId="user-1"
        allProjectIds={["project-a", "project-b"]}
      />,
    );
    expect(screen.getByText("Reason: Project A maintenance")).toBeInTheDocument();

    rerender(
      <DispatchPauseBanner
        entries={entries}
        selectedProjectId={ALL_PROJECTS}
        currentUserId="user-1"
        allProjectIds={["project-a", "project-b"]}
      />,
    );
    expect(screen.getByText("Reason: Project A maintenance")).toBeInTheDocument();
  });

  it("shows a current-user pause regardless of current project", () => {
    render(
      <DispatchPauseBanner
        entries={[
          pause("user", "user-1", "Current user pause", "lead"),
          pause("user", "user-2", "Other user pause"),
        ]}
        selectedProjectId="unrelated-project"
        currentUserId="user-1"
      />,
    );

    expect(screen.getByText("Reason: Current user pause")).toBeInTheDocument();
    expect(screen.queryByText("Reason: Other user pause")).not.toBeInTheDocument();
  });

  it("shows multiple applicable pauses without losing metadata or safety copy", () => {
    render(
      <DispatchPauseBanner
        entries={[
          pause("global", null, "Fleet hold", "admin"),
          pause("project", "project-a", "Repository hold", "maintainer"),
          pause("user", "user-1", "User hold", "pm"),
        ]}
        selectedProjectId="project-a"
        currentUserId="user-1"
        allProjectIds={["project-a"]}
      />,
    );

    expect(screen.getByText(/Running sessions and chat are unaffected/i)).toBeInTheDocument();
    expect(screen.getByText(/new dispatch is deferred/i)).toBeInTheDocument();
    expect(screen.getByText("Reason: Fleet hold")).toBeInTheDocument();
    expect(screen.getByText("Paused by admin")).toBeInTheDocument();
    expect(screen.getByText("Reason: Repository hold")).toBeInTheDocument();
    expect(screen.getByText("Paused by maintainer")).toBeInTheDocument();
    expect(screen.getByText("Reason: User hold")).toBeInTheDocument();
    expect(screen.getByText("Paused by pm")).toBeInTheDocument();
  });

  it("reacts to SSE upsert and resume clearing store state without a refresh", () => {
    render(<DispatchPauseBanner selectedProjectId="project-a" currentUserId="user-1" />);

    expect(screen.queryByRole("status", { name: "Dispatch paused" })).not.toBeInTheDocument();

    act(() => {
      dispatchPauseStore.getState().applySsePayload({
        scope: "project",
        target_id: "project-a",
        current: {
          paused_by: "admin",
          paused_at: "2026-01-01T12:00:00Z",
          reason: "SSE pause",
        },
      });
    });

    expect(screen.getByText("Reason: SSE pause")).toBeInTheDocument();

    act(() => {
      dispatchPauseStore.getState().applySsePayload({
        scope: "project",
        target_id: "project-a",
        current: null,
        previous: {
          paused_by: "admin",
          paused_at: "2026-01-01T12:00:00Z",
          reason: "SSE pause",
        },
        resumed_by: "admin",
      });
    });

    expect(screen.queryByRole("status", { name: "Dispatch paused" })).not.toBeInTheDocument();
  });
});
