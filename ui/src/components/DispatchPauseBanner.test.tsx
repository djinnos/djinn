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

const statusBanner = () => screen.getByRole("status", { name: "Dispatch paused" });

describe("DispatchPauseBanner", () => {
  beforeEach(() => {
    dispatchPauseStore.getState().clearAll();
    projectStore.setState({
      selectedProjectId: "project-a",
      projects: [
        { id: "project-a", name: "Project A", github_owner: "acme", github_repo: "alpha" },
        { id: "project-b", name: "Project B", github_owner: "acme", github_repo: "beta" },
      ],
    });
  });

  it("renders nothing when no pause applies", () => {
    const { container } = render(
      <DispatchPauseBanner entries={[]} selectedProjectId="project-a" currentUserId="user-1" />,
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

    const banner = statusBanner();
    expect(banner).toHaveTextContent("Global dispatch pause");
    expect(banner).toHaveTextContent("Global maintenance");
    expect(banner).toHaveTextContent("Paused by: admin-1");
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

    expect(screen.queryByText(/Project A maintenance/)).not.toBeInTheDocument();

    rerender(
      <DispatchPauseBanner
        entries={entries}
        selectedProjectId="project-a"
        currentUserId="user-1"
        allProjectIds={["project-a", "project-b"]}
      />,
    );
    expect(statusBanner()).toHaveTextContent("Project A maintenance");

    rerender(
      <DispatchPauseBanner
        entries={entries}
        selectedProjectId={ALL_PROJECTS}
        currentUserId="user-1"
        allProjectIds={["project-a", "project-b"]}
      />,
    );
    expect(statusBanner()).toHaveTextContent("Project A maintenance");
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

    const banner = statusBanner();
    expect(banner).toHaveTextContent("Current user pause");
    expect(banner).not.toHaveTextContent("Other user pause");
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

    const banner = statusBanner();
    expect(banner).toHaveTextContent(/Running sessions and chat are unaffected/i);
    expect(banner).toHaveTextContent(/new dispatch is deferred/i);
    expect(banner).toHaveTextContent("Fleet hold");
    expect(banner).toHaveTextContent("Paused by: admin");
    expect(banner).toHaveTextContent("Repository hold");
    expect(banner).toHaveTextContent("Paused by: maintainer");
    expect(banner).toHaveTextContent("User hold");
    expect(banner).toHaveTextContent("Paused by: pm");
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

    expect(statusBanner()).toHaveTextContent("SSE pause");

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
