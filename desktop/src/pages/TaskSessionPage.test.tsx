import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen } from "@/test/test-utils";
import { TaskSessionPage } from "./TaskSessionPage";
import type { Task } from "@/api/types";

const mockTask: Task = {
  id: "task-1",
  short_id: "T-1",
  title: "Task One",
  description: "Task description",
  status: "in_progress",
  owner: null,
  priority: 1,
  issue_type: "task",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
  project_id: "project-1",
  acceptance_criteria: [{ criterion: "Do the thing", met: false }],
};

const useTaskStoreMock = vi.fn();
const buildSetupVerificationViewMock = vi.fn();

vi.mock("react-router-dom", async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>();
  return {
    ...actual,
    useParams: () => ({ taskId: "task-1" }),
    useNavigate: () => vi.fn(),
  };
});

vi.mock("@/stores/useProjectStore", () => ({
  useSelectedProject: () => ({ id: "p1", name: "Test", path: "/tmp/test" }),
}));

vi.mock("@/stores/useTaskStore", () => ({
  useTaskStore: (selector: (state: { tasks: Map<string, Task> }) => unknown) =>
    useTaskStoreMock(selector),
}));

vi.mock("@/stores/taskStore", () => ({
  taskStore: {
    subscribe: () => () => {},
  },
}));

vi.mock("@/stores/useEpicStore", () => ({
  useEpicStore: (selector: (state: { epics: Map<string, unknown> }) => unknown) =>
    selector({ epics: new Map() }),
}));

vi.mock("@/hooks/useSessionMessages", () => ({
  useSessionMessages: () => ({
    timeline: [],
    sessions: [],
    loading: false,
    error: null,
    streamingText: "",
  }),
}));

vi.mock("@/components/SessionThread", () => ({
  SessionThread: () => <div data-testid="session-thread" />,
}));

vi.mock("@/lib/setupVerificationView", async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>();
  return {
    ...actual,
    buildSetupVerificationView: (...args: unknown[]) => buildSetupVerificationViewMock(...args),
  };
});

describe("TaskSessionPage sidebar Setup & Verification", () => {
  beforeEach(() => {
    useTaskStoreMock.mockImplementation((selector: (state: { tasks: Map<string, Task> }) => unknown) =>
      selector({ tasks: new Map([[mockTask.id, mockTask]]) })
    );
    buildSetupVerificationViewMock.mockReset();
    buildSetupVerificationViewMock.mockReturnValue({
      taskId: mockTask.id,
      steps: [],
      status: "passed",
      hasData: false,
      allPassed: false,
      isRunning: false,
      hasFailed: false,
      totalDuration: 0,
      failedStepId: null,
    });
  });

  it("renders Setup & Verification when step history exists", () => {
    buildSetupVerificationViewMock.mockReturnValue({
      taskId: mockTask.id,
      steps: [
        {
          index: 0,
          name: "Dispatch",
          phase: "setup",
          status: "running",
        },
      ],
      status: "running",
      hasData: true,
      allPassed: false,
      isRunning: true,
      hasFailed: false,
      totalDuration: 0,
      failedStepId: null,
    });

    render(<TaskSessionPage />);

    expect(screen.getByText("Acceptance Criteria")).toBeInTheDocument();
  });

  it("does not render Setup & Verification section when there is no dispatch/step history", () => {
    render(<TaskSessionPage />);
    expect(screen.queryByText("Setup & Verification")).not.toBeInTheDocument();
  });
});

