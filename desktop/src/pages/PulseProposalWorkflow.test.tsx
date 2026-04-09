import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, render, userEvent, waitFor, within } from "@/test/test-utils";
import { PulsePage } from "@/pages/PulsePage";
import { KanbanBoard } from "@/components/KanbanBoard";
import { Sidebar } from "@/components/Sidebar";
import { callMcpTool } from "@/api/mcpClient";
import { projectStore } from "@/stores/projectStore";
import { epicStore } from "@/stores/epicStore";
import { taskStore } from "@/stores/taskStore";
import { useAuthStore } from "@/stores/authStore";

const { mockNavigate } = vi.hoisted(() => ({
  mockNavigate: vi.fn(),
}));

vi.mock("react-router-dom", async () => {
  const actual = await vi.importActual<typeof import("react-router-dom")>("react-router-dom");
  return {
    ...actual,
    useNavigate: () => mockNavigate,
  };
});

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: vi.fn(),
}));

vi.mock("@/lib/toast", () => ({
  showToast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

vi.mock("@/components/pulse/HotspotsPanel", () => ({
  HotspotsPanel: () => <div data-testid="hotspots-panel" />,
}));

vi.mock("@/components/pulse/DeadCodePanel", () => ({
  DeadCodePanel: () => <div data-testid="deadcode-panel" />,
}));

vi.mock("@/components/pulse/CyclesPanel", () => ({
  CyclesPanel: () => <div data-testid="cycles-panel" />,
}));

vi.mock("@/components/pulse/BlastRadiusPanel", () => ({
  BlastRadiusPanel: () => <div data-testid="blast-radius-panel" />,
}));

vi.mock("@/components/pulse/PulseSettingsSheet", () => ({
  PulseSettingsSheet: () => <div data-testid="pulse-settings-sheet" />,
}));

vi.mock("@/hooks/useExecutionStatus", () => ({
  useExecutionStatus: () => ({ state: "idle", refresh: vi.fn() }),
}));

vi.mock("@/hooks/useExecutionControl", () => ({
  useExecutionControl: () => ({ start: vi.fn(), pause: vi.fn(), resume: vi.fn() }),
}));

describe("Pulse architect proposal workflow", () => {
  beforeEach(() => {
    localStorage.clear();
    mockNavigate.mockReset();
    vi.mocked(callMcpTool).mockReset();
    taskStore.getState().clearTasks();
    epicStore.getState().clearEpics();
    projectStore.setState({
      projects: [{ id: "project-a", name: "Project Alpha", path: "/tmp/project-alpha" }],
      selectedProjectId: "project-a",
      lastViewPerProject: {},
    });
    useAuthStore.setState({
      isAuthenticated: true,
      user: { sub: "user-1", email: "fernando@example.com", name: "Fernando" },
      isLoading: false,
      error: null,
    });
  });

  it("creates an architect spike from Pulse, reviews the draft in-place, and accepts it into an epic handoff", async () => {
    const user = userEvent.setup();

    let draftVisible = false;
    let proposalAccepted = false;

    vi.mocked(callMcpTool).mockImplementation(async (toolName, args) => {
      if (toolName === "code_graph") {
        return {
          project_id: "project-a",
          warmed: true,
          last_warm_at: "2026-04-09T11:45:00Z",
          pinned_commit: "abc123",
          commits_since_pin: 0,
        } as never;
      }

      if (toolName === "session_active") {
        return { sessions: [] } as never;
      }

      if (toolName === "task_create") {
        draftVisible = true;
        return {
          id: "spike-1",
          title: "How should we roll out observability across the platform?",
          status: "open",
          description: "## Question\nHow should we roll out observability across the platform?",
          owner: null,
          labels: [],
          acceptance_criteria: [],
          created_at: "2026-04-09T12:00:00Z",
          updated_at: "2026-04-09T12:00:00Z",
          issue_type: "spike",
        } as never;
      }

      if (toolName === "propose_adr_list") {
        return {
          items: draftVisible && !proposalAccepted
            ? [
                {
                  id: "adr-observability",
                  title: "Observability rollout proposal",
                  path: "/tmp/adr-observability.md",
                  work_shape: "epic",
                  originating_spike_id: "spike-1",
                  mtime: "2026-04-09T12:05:00Z",
                },
              ]
            : [],
        } as never;
      }

      if (toolName === "propose_adr_show") {
        return {
          adr: {
            id: "adr-observability",
            title: "Observability rollout proposal",
            path: "/tmp/adr-observability.md",
            work_shape: "epic",
            body: "# Observability rollout proposal\n\n## Summary\nShip a project-wide observability baseline.\n\n## Plan\n1. Stand up telemetry pipeline\n2. Add dashboards\n3. Break down work for rollout",
            originating_spike_id: "spike-1",
            mtime: "2026-04-09T12:05:00Z",
          },
        } as never;
      }

      if (toolName === "propose_adr_accept") {
        proposalAccepted = true;

        const epic = {
          id: "epic-observability",
          short_id: "e-obsv",
          title: "Observability rollout",
          description: "Epic shell for observability rollout",
          emoji: "🛰️",
          color: "#7dd3fc",
          status: "open",
          owner: null,
          created_at: "2026-04-09T12:06:00Z",
          updated_at: "2026-04-09T12:06:00Z",
          memory_refs: [],
          auto_breakdown: true,
          originating_adr_id: "adr-observability",
        };

        epicStore.getState().setEpics([epic]);
        taskStore.getState().setTasks([
          {
            id: "task-telemetry",
            title: "Implement telemetry ingestion",
            status: "open",
            description: "",
            owner: null,
            priority: 1,
            issue_type: "task",
            created_at: "2026-04-09T12:06:30Z",
            updated_at: "2026-04-09T12:06:30Z",
            epic_id: epic.id,
            labels: [],
          },
          {
            id: "task-dashboard",
            title: "Create rollout dashboards",
            status: "open",
            description: "",
            owner: null,
            priority: 1,
            issue_type: "task",
            created_at: "2026-04-09T12:06:45Z",
            updated_at: "2026-04-09T12:06:45Z",
            epic_id: epic.id,
            labels: [],
          },
        ]);

        return {
          accepted_path: "/accepted/adr-observability.md",
          epic,
        } as never;
      }

      throw new Error(`Unexpected MCP tool call: ${toolName} ${JSON.stringify(args)}`);
    });

    const firstRender = render(<PulsePage />);

    await user.click(await screen.findByRole("button", { name: "Ask architect" }));
    await user.type(
      screen.getByLabelText("Question"),
      "How should we roll out observability across the platform?",
    );
    await user.type(
      screen.getByLabelText("Context"),
      "We need an epic shell and immediate planner breakdown for Q2 delivery.",
    );
    await user.click(screen.getByRole("button", { name: "Create spike" }));

    await waitFor(() => {
      expect(callMcpTool).toHaveBeenCalledWith(
        "task_create",
        expect.objectContaining({
          project: "/tmp/project-alpha",
          issue_type: "spike",
          title: "How should we roll out observability across the platform?",
          description: expect.stringContaining("## Context"),
        }),
      );
    });

    expect(mockNavigate).toHaveBeenCalledWith("/task/spike-1");
    expect(taskStore.getState().getTask("spike-1")).toMatchObject({
      id: "spike-1",
      issue_type: "spike",
    });

    firstRender.unmount();

    render(<PulsePage />);

    render(<Sidebar />, {
      wrapperOptions: {
        routerProps: {
          initialEntries: ["/projects/project-a/pulse"],
        },
      },
    });

    expect(await screen.findByLabelText("Pulse has 1 pending proposals")).toBeInTheDocument();

    const proposalCard = await screen.findByText("Observability rollout proposal");
    await user.click(proposalCard.closest("button") as HTMLButtonElement);

    const detailPanel = await screen.findByLabelText("Proposal detail panel");
    expect(mockNavigate).toHaveBeenCalledTimes(1);
    expect(within(detailPanel).getByText("Proposal ID")).toBeInTheDocument();
    expect(within(detailPanel).getByText("Ship a project-wide observability baseline.")).toBeInTheDocument();
    expect(within(detailPanel).getByText("Break down work for rollout")).toBeInTheDocument();

    await waitFor(() => {
      expect(callMcpTool).toHaveBeenCalledWith(
        "propose_adr_show",
        expect.objectContaining({
          project: "/tmp/project-alpha",
          id: "adr-observability",
        }),
      );
    });

    await user.click(within(detailPanel).getByRole("button", { name: "Accept" }));
    await user.click(within(detailPanel).getByRole("button", { name: "Confirm accept" }));

    await waitFor(() => {
      expect(callMcpTool).toHaveBeenCalledWith(
        "propose_adr_accept",
        expect.objectContaining({
          project: "/tmp/project-alpha",
          id: "adr-observability",
          title: "Observability rollout proposal",
          create_epic: true,
          auto_breakdown: true,
        }),
      );
    });

    await waitFor(() => {
      expect(screen.getByText("No pending architect proposals")).toBeInTheDocument();
    });

    expect(epicStore.getState().getEpic("epic-observability")).toMatchObject({
      title: "Observability rollout",
      status: "open",
      auto_breakdown: true,
      originating_adr_id: "adr-observability",
    });

    render(<KanbanBoard disableSearchParamSync />);

    expect(await screen.findByText("Observability rollout")).toBeInTheDocument();
    expect(screen.getByText("Implement telemetry ingestion")).toBeInTheDocument();
    expect(screen.getByText("Create rollout dashboards")).toBeInTheDocument();
  });

  it("reviews a draft in-place and rejects it with threaded feedback", async () => {
    const user = userEvent.setup();

    let proposalRejected = false;

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "code_graph") {
        return {
          project_id: "project-a",
          warmed: true,
          last_warm_at: "2026-04-09T11:45:00Z",
          pinned_commit: "abc123",
          commits_since_pin: 0,
        } as never;
      }

      if (toolName === "session_active") {
        return { sessions: [] } as never;
      }

      if (toolName === "propose_adr_list") {
        return {
          items: proposalRejected
            ? []
            : [
                {
                  id: "adr-reject-me",
                  title: "Incomplete architecture proposal",
                  path: "/tmp/adr-reject-me.md",
                  work_shape: "architectural",
                  originating_spike_id: "spike-2",
                  mtime: "2026-04-09T12:10:00Z",
                },
              ],
        } as never;
      }

      if (toolName === "propose_adr_show") {
        return {
          adr: {
            id: "adr-reject-me",
            title: "Incomplete architecture proposal",
            path: "/tmp/adr-reject-me.md",
            work_shape: "architectural",
            body: "# Incomplete architecture proposal\n\nNeeds constraints, rollout plan, and trade-offs.",
            originating_spike_id: "spike-2",
            mtime: "2026-04-09T12:10:00Z",
          },
        } as never;
      }

      if (toolName === "propose_adr_reject") {
        proposalRejected = true;
        return {
          ok: true,
          feedback_target: "task/spike-2",
        } as never;
      }

      throw new Error(`Unexpected MCP tool call: ${toolName}`);
    });

    render(<PulsePage />);

    const proposalCard = await screen.findByText("Incomplete architecture proposal");
    await user.click(proposalCard.closest("button") as HTMLButtonElement);

    const detailPanel = await screen.findByLabelText("Proposal detail panel");
    expect(within(detailPanel).getByText("Needs constraints, rollout plan, and trade-offs.")).toBeInTheDocument();

    await waitFor(() => {
      expect(callMcpTool).toHaveBeenCalledWith(
        "propose_adr_show",
        expect.objectContaining({
          project: "/tmp/project-alpha",
          id: "adr-reject-me",
        }),
      );
    });

    await user.click(within(detailPanel).getByRole("button", { name: "Reject" }));
    await user.type(within(detailPanel).getByLabelText("Reason"), "Please add trade-offs and rollout sequencing.");
    await user.click(within(detailPanel).getByRole("button", { name: "Confirm reject" }));

    await waitFor(() => {
      expect(callMcpTool).toHaveBeenCalledWith(
        "propose_adr_reject",
        expect.objectContaining({
          project: "/tmp/project-alpha",
          id: "adr-reject-me",
          reason: "Please add trade-offs and rollout sequencing.",
        }),
      );
    });

    await waitFor(() => {
      expect(screen.getByText("No pending architect proposals")).toBeInTheDocument();
    });
  });
});
