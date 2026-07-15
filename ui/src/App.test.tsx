import { describe, it, expect, beforeEach, vi } from "vitest";
import { render, screen } from "@/test/test-utils";
import { MainLayout } from "./App";
import { projectStore } from "@/stores/projectStore";

// Mock heavy child components — we only care about ProjectSelector visibility
vi.mock("@/components/Sidebar", () => ({
  Sidebar: () => <div data-testid="sidebar" />,
}));
vi.mock("@/components/ConnectionBanner", () => ({
  ConnectionBanner: () => null,
}));
vi.mock("@/components/DispatchPauseBanner", () => ({
  DispatchPauseBanner: () => null,
}));

// Mock all page components to lightweight stubs
vi.mock("@/pages/KanbanPage", () => ({
  KanbanPage: () => <div>KanbanPage</div>,
}));
vi.mock("@/pages/DependenciesPage", () => ({
  DependenciesPage: () => <div>DependenciesPage</div>,
}));
vi.mock("@/pages/AgentsPage", () => ({
  AgentsPage: () => <div>AgentsPage</div>,
}));
vi.mock("@/pages/SettingsPage", () => ({
  SettingsPage: () => <div>SettingsPage</div>,
}));
vi.mock("@/pages/TaskSessionPage", () => ({
  TaskSessionPage: () => <div>TaskSessionPage</div>,
}));
vi.mock("@/pages/ChatPage", () => ({
  ChatPage: () => <div>ChatPage</div>,
}));
vi.mock("@/pages/MemoryPage", () => ({
  MemoryPage: () => <div>MemoryPage</div>,
}));
vi.mock("@/pages/CodeGraphPage", () => ({
  CodeGraphPage: () => <div>CodeGraphPage</div>,
}));
vi.mock("@/pages/ProposalsPage", () => ({
  ProposalsPage: () => <div>ProposalsPage</div>,
}));
vi.mock("@/pages/RepositoriesPage", () => ({
  RepositoriesPage: () => <div>RepositoriesPage</div>,
}));
vi.mock("@/pages/ImagesPage", () => ({
  ImagesPage: () => <div>ImagesPage</div>,
}));
vi.mock("@/pages/ImageEditorPage", () => ({
  ImageEditorPage: () => <div>ImageEditorPage</div>,
}));
vi.mock("@/pages/ProjectEnvironmentPage", () => ({
  ProjectEnvironmentPage: () => <div>ProjectEnvironmentPage</div>,
}));
vi.mock("@/pages/UsersPage", () => ({
  UsersPage: () => <div>UsersPage</div>,
}));
vi.mock("@/pages/UsageDashboardPage", () => ({
  UsageDashboardPage: () => <div>UsageDashboardPage</div>,
}));
vi.mock("@/components/board/BoardLayout", () => ({
  BoardLayout: () => <div>BoardLayout</div>,
}));

describe("MainLayout — project selector chrome", () => {
  beforeEach(() => {
    projectStore.setState({
      projects: [
        { id: "proj-a", name: "Project Alpha", path: "/tmp/a" },
        { id: "proj-b", name: "Project Beta", path: "/tmp/b" },
      ],
      selectedProjectId: "proj-a",
      lastViewPerProject: {},
    });
  });

  it("renders the shared project selector on /agents (global-project-context)", () => {
    render(<MainLayout />, { wrapperOptions: { routerProps: { initialEntries: ["/agents"] } } });
    const selector = screen.getByLabelText("Select project");
    expect(selector).toBeInTheDocument();
  });

  it("renders the shared project selector on /task/:taskId (global-project-context)", () => {
    render(<MainLayout />, { wrapperOptions: { routerProps: { initialEntries: ["/task/abc-123"] } } });
    const selector = screen.getByLabelText("Select project");
    expect(selector).toBeInTheDocument();
  });

  it("renders the shared project selector on /memory (global-project-context)", () => {
    render(<MainLayout />, { wrapperOptions: { routerProps: { initialEntries: ["/memory"] } } });
    const selector = screen.getByLabelText("Select project");
    expect(selector).toBeInTheDocument();
  });

  it("does NOT render the project selector on /code-graph (galaxy HUD has its own chip)", () => {
    render(<MainLayout />, { wrapperOptions: { routerProps: { initialEntries: ["/code-graph"] } } });
    const selector = screen.queryByLabelText("Select project");
    expect(selector).not.toBeInTheDocument();
  });

  it("does NOT render the project selector on /tasks (url-filtered board route)", () => {
    render(<MainLayout />, { wrapperOptions: { routerProps: { initialEntries: ["/tasks"] } } });
    const selector = screen.queryByLabelText("Select project");
    expect(selector).not.toBeInTheDocument();
  });

  it("does NOT render the project selector on /dependencies (url-filtered board route)", () => {
    render(<MainLayout />, { wrapperOptions: { routerProps: { initialEntries: ["/dependencies"] } } });
    const selector = screen.queryByLabelText("Select project");
    expect(selector).not.toBeInTheDocument();
  });

  it("does NOT render the project selector on /admin/usage (url-filtered)", () => {
    render(<MainLayout />, { wrapperOptions: { routerProps: { initialEntries: ["/admin/usage"] } } });
    const selector = screen.queryByLabelText("Select project");
    expect(selector).not.toBeInTheDocument();
  });

  it("does NOT render the project selector on /proposals (global)", () => {
    render(<MainLayout />, { wrapperOptions: { routerProps: { initialEntries: ["/proposals"] } } });
    const selector = screen.queryByLabelText("Select project");
    expect(selector).not.toBeInTheDocument();
  });

  it("does NOT render the project selector on /chat (global)", () => {
    render(<MainLayout />, { wrapperOptions: { routerProps: { initialEntries: ["/chat"] } } });
    const selector = screen.queryByLabelText("Select project");
    expect(selector).not.toBeInTheDocument();
  });

  it("does NOT render the project selector on /settings (global)", () => {
    render(<MainLayout />, { wrapperOptions: { routerProps: { initialEntries: ["/settings"] } } });
    const selector = screen.queryByLabelText("Select project");
    expect(selector).not.toBeInTheDocument();
  });

  it("does NOT render the project selector on /projects/:id/environment (path-scoped)", () => {
    render(<MainLayout />, { wrapperOptions: { routerProps: { initialEntries: ["/projects/proj-1/environment"] } } });
    const selector = screen.queryByLabelText("Select project");
    expect(selector).not.toBeInTheDocument();
  });
});
