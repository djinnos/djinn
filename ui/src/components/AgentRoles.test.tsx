import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent } from "@testing-library/react";
import { AgentRoles } from "./AgentRoles";
import { render, screen, userEvent, waitFor, within } from "@/test/test-utils";
import {
  createAgent,
  deleteAgent,
  fetchAgents,
  fetchAvailableMcpServers,
  fetchAvailableSkills,
  updateAgent,
  type Agent,
} from "@/api/agents";

const mockState = vi.hoisted(() => ({
  selectedProject: {
    id: "project-1",
    name: "Djinn",
    github_owner: "djinnos",
    github_repo: "djinn",
    path: "/workspace/djinn",
  },
  authUser: {
    id: "user-1",
    login: "admin",
    name: "Admin User",
    avatarUrl: null,
    isAdmin: true,
    role: "engineer",
  },
}));

vi.mock("@/stores/useProjectStore", async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>();
  return {
    ...actual,
    useSelectedProject: () => mockState.selectedProject,
  };
});

vi.mock("@/components/AuthGate", async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>();
  return {
    ...actual,
    useAuthUser: () => mockState.authUser,
  };
});

vi.mock("@/api/agents", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/agents")>();
  return {
    ...actual,
    fetchAgents: vi.fn(),
    fetchAvailableMcpServers: vi.fn(),
    fetchAvailableSkills: vi.fn(),
    createAgent: vi.fn(),
    updateAgent: vi.fn(),
    deleteAgent: vi.fn(),
  };
});

const makeAgent = (overrides: Partial<Agent>): Agent => ({
  id: "agent-1",
  name: "Default Worker",
  base_role: "worker",
  description: "Handles implementation tasks",
  system_prompt_extensions: [],
  mcp_servers: [],
  skills: [],
  model_preference: null,
  is_default: false,
  ...overrides,
});

const defaultWorker = makeAgent({
  id: "default-worker",
  name: "Default Worker",
  base_role: "worker",
  description: "Handles implementation tasks",
  is_default: true,
  system_prompt_extensions: ["Prefer small, focused changes."],
  mcp_servers: ["github"],
  skills: ["rust-review"],
});

const reviewerSpecialist = makeAgent({
  id: "reviewer-specialist",
  name: "Strict Reviewer",
  base_role: "reviewer",
  description: "Reviews risky backend changes",
  system_prompt_extensions: ["Check migrations.", "Check concurrency."],
  mcp_servers: ["github", "postgres"],
  skills: ["rust-review", "testing-library"],
});

const architectDefault = makeAgent({
  id: "default-architect",
  name: "Default Architect",
  base_role: "architect",
  description: "Designs system architecture",
  is_default: true,
});

describe("AgentRoles", () => {
  beforeEach(() => {
    vi.mocked(fetchAgents).mockReset();
    vi.mocked(fetchAvailableMcpServers).mockReset();
    vi.mocked(fetchAvailableSkills).mockReset();
    vi.mocked(createAgent).mockReset();
    vi.mocked(updateAgent).mockReset();
    vi.mocked(deleteAgent).mockReset();

    mockState.selectedProject = {
      id: "project-1",
      name: "Djinn",
      github_owner: "djinnos",
      github_repo: "djinn",
      path: "/workspace/djinn",
    };
    mockState.authUser = {
      id: "user-1",
      login: "admin",
      name: "Admin User",
      avatarUrl: null,
      isAdmin: true,
      role: "engineer",
    };

    vi.mocked(fetchAvailableMcpServers).mockResolvedValue([
      { name: "github", transport: "stdio" },
      { name: "postgres", transport: "sse" },
    ]);
    vi.mocked(fetchAvailableSkills).mockResolvedValue([
      { name: "rust-review", description: "Review Rust code" },
      { name: "testing-library", description: "Exercise UI flows" },
    ]);
  });

  it("groups project defaults separately from specialists with explanatory copy", async () => {
    vi.mocked(fetchAgents).mockResolvedValue([defaultWorker, reviewerSpecialist, architectDefault]);

    render(<AgentRoles />);

    expect(screen.getByText("Loading roles...")).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "Agent Roles" })).toBeInTheDocument();

    expect(fetchAgents).toHaveBeenCalledWith("project-1");

    const defaultsSection = screen.getByRole("region", { name: "Project defaults" });
    const specialistsSection = screen.getByRole("region", { name: "Specialists" });

    expect(defaultsSection).toHaveTextContent(
      /used automatically for worker, planner, lead, reviewer, and architect dispatch/i,
    );
    expect(defaultsSection).toHaveTextContent(
      /customize the default behavior for this project/i,
    );
    expect(specialistsSection).toHaveTextContent(
      /run only when a task routes to that specialist agent type or name/i,
    );
    expect(specialistsSection).toHaveTextContent(/New Specialist creates specialist-only agents/i);

    expect(within(defaultsSection).getByText("Handles implementation tasks")).toBeInTheDocument();
    expect(within(defaultsSection).getByText("Designs system architecture")).toBeInTheDocument();
    expect(within(defaultsSection).getAllByText("default").length).toBe(2);
    expect(within(defaultsSection).queryByText("Strict Reviewer")).not.toBeInTheDocument();
    expect(within(defaultsSection).getByText("1 ext")).toBeInTheDocument();
    expect(within(defaultsSection).getByText("Worker")).toBeInTheDocument();
    expect(within(defaultsSection).getByText("Architect")).toBeInTheDocument();
    expect(within(defaultsSection).getByText("1 MCP")).toBeInTheDocument();
    expect(within(defaultsSection).getByText("1 skill")).toBeInTheDocument();
    expect(within(defaultsSection).getAllByRole("button", { name: "Edit instructions" })).toHaveLength(2);
    expect(within(defaultsSection).queryByRole("button", { name: "Edit" })).not.toBeInTheDocument();
    expect(
      within(defaultsSection).queryByRole("button", { name: /delete/i }),
    ).not.toBeInTheDocument();

    expect(within(specialistsSection).getByText("Strict Reviewer")).toBeInTheDocument();
    expect(within(specialistsSection).getByText("Reviews risky backend changes")).toBeInTheDocument();
    expect(within(specialistsSection).getByText("2 exts")).toBeInTheDocument();
    expect(within(specialistsSection).getByText("Reviewer")).toBeInTheDocument();
    expect(within(specialistsSection).getByText("2 MCP")).toBeInTheDocument();
    expect(within(specialistsSection).getByText("2 skills")).toBeInTheDocument();
    expect(within(specialistsSection).getByRole("button", { name: "Edit" })).toBeInTheDocument();
  });

  it("keeps specialist creation and empty copy in the specialists section", async () => {
    vi.mocked(fetchAgents).mockResolvedValue([defaultWorker, architectDefault]);

    render(<AgentRoles />);

    const specialistsSection = await screen.findByRole("region", { name: "Specialists" });
    expect(within(specialistsSection).getByRole("button", { name: "New Specialist" })).toBeInTheDocument();
    expect(specialistsSection).toHaveTextContent(
      /Create a specialist only when tasks should explicitly route to a custom agent type or name/i,
    );
    expect(specialistsSection).toHaveTextContent(/this does not edit project defaults/i);
  });

  it("creates a specialist from the form with selected capabilities", async () => {
    const user = userEvent.setup();
    const created = makeAgent({
      id: "created-agent",
      name: "Frontend Reviewer",
      base_role: "reviewer",
      description: "Reviews React UI changes",
      system_prompt_extensions: ["Prefer accessible queries."],
      mcp_servers: ["github"],
      skills: ["testing-library"],
    });

    vi.mocked(fetchAgents).mockResolvedValue([defaultWorker]);
    vi.mocked(createAgent).mockResolvedValue(created);

    render(<AgentRoles />);

    const specialistsSection = await screen.findByRole("region", { name: "Specialists" });
    await user.click(within(specialistsSection).getByRole("button", { name: "New Specialist" }));

    expect(await screen.findByRole("heading", { name: "New specialist" })).toBeInTheDocument();
    expect(fetchAvailableMcpServers).toHaveBeenCalledWith("project-1");
    expect(fetchAvailableSkills).toHaveBeenCalledWith("project-1");

    await user.click(screen.getByRole("button", { name: "Task Reviewer" }));
    await user.type(screen.getByLabelText("Name"), "Frontend Reviewer");
    await user.type(screen.getByLabelText("Description"), "Reviews React UI changes");
    await user.type(screen.getByLabelText("System prompt extensions"), "Prefer accessible queries.");

    await waitFor(() => {
      expect(screen.getByRole("option", { name: "github (stdio)" })).toBeInTheDocument();
      expect(
        screen.getByRole("option", { name: "testing-library — Exercise UI flows" }),
      ).toBeInTheDocument();
    });

    const capabilitySelectors = screen.getAllByRole("combobox");
    fireEvent.change(capabilitySelectors[0], { target: { value: "github" } });
    fireEvent.change(capabilitySelectors[1], { target: { value: "testing-library" } });

    await user.click(screen.getByRole("button", { name: "Create" }));

    await waitFor(() => {
      expect(createAgent).toHaveBeenCalledWith({
        project_id: "project-1",
        base_role: "reviewer",
        name: "Frontend Reviewer",
        description: "Reviews React UI changes",
        system_prompt_extensions: ["Prefer accessible queries."],
        mcp_servers: ["github"],
        skills: ["testing-library"],
      });
    });

    expect(await screen.findByText("Frontend Reviewer")).toBeInTheDocument();
    expect(screen.getByText("Reviews React UI changes")).toBeInTheDocument();
  });

  it("updates an existing specialist through the edit form", async () => {
    const user = userEvent.setup();
    vi.mocked(fetchAgents).mockResolvedValue([reviewerSpecialist]);
    vi.mocked(updateAgent).mockResolvedValue({
      ...reviewerSpecialist,
      name: "Careful Reviewer",
      description: "Reviews high-risk backend changes",
      skills: ["testing-library"],
    });

    render(<AgentRoles />);

    const card = await screen.findByText("Strict Reviewer");
    const cardRoot = card.closest(".group") as HTMLElement;
    await user.click(within(cardRoot).getByRole("button", { name: "Edit" }));

    expect(await screen.findByRole("heading", { name: 'Edit "Strict Reviewer"' })).toBeInTheDocument();
    await user.clear(screen.getByLabelText("Name"));
    await user.type(screen.getByLabelText("Name"), "Careful Reviewer");
    await user.clear(screen.getByLabelText("Description"));
    await user.type(screen.getByLabelText("Description"), "Reviews high-risk backend changes");

    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => {
      expect(updateAgent).toHaveBeenCalledWith("reviewer-specialist", {
        name: "Careful Reviewer",
        description: "Reviews high-risk backend changes",
        system_prompt_extensions: ["Check migrations.", "Check concurrency."],
        mcp_servers: ["github", "postgres"],
        skills: ["rust-review", "testing-library"],
      });
    });

    expect(await screen.findByText("Careful Reviewer")).toBeInTheDocument();
    expect(screen.getByText("Reviews high-risk backend changes")).toBeInTheDocument();
  });

  it("edits default-agent instructions without submitting immutable identity fields", async () => {
    const user = userEvent.setup();
    vi.mocked(fetchAgents).mockResolvedValue([defaultWorker]);
    vi.mocked(updateAgent).mockResolvedValue({
      ...defaultWorker,
      system_prompt_extensions: ["Prefer small, focused changes.", "Run focused tests."],
    });

    render(<AgentRoles />);

    const defaultsSection = await screen.findByRole("region", { name: "Project defaults" });
    await user.click(within(defaultsSection).getByRole("button", { name: "Edit instructions" }));

    expect(
      await screen.findByRole("heading", { name: "Edit default Worker instructions" }),
    ).toBeInTheDocument();
    expect(screen.getByText(/Identity fields are immutable/i)).toBeInTheDocument();
    expect(screen.getByText("Default Worker")).toBeInTheDocument();
    expect(screen.getByText("Project default")).toBeInTheDocument();
    expect(screen.queryByLabelText("Name")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Task Reviewer" })).not.toBeInTheDocument();

    const instructions = screen.getByLabelText("Default instructions");
    await user.type(instructions, "\nRun focused tests.");

    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => {
      expect(updateAgent).toHaveBeenCalledWith("default-worker", {
        system_prompt_extensions: ["Prefer small, focused changes.", "Run focused tests."],
        mcp_servers: ["github"],
        skills: ["rust-review"],
      });
    });

    const payload = vi.mocked(updateAgent).mock.calls[0][1];
    expect(payload).not.toHaveProperty("name");
    expect(payload).not.toHaveProperty("description");
    expect(payload).not.toHaveProperty("base_role");
    expect(payload).not.toHaveProperty("is_default");
  });

  it("renders an error state when the initial role load fails", async () => {
    vi.mocked(fetchAgents).mockRejectedValue(new Error("agents exploded"));

    render(<AgentRoles />);

    expect(await screen.findByText("agents exploded")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Agent Roles" })).toBeInTheDocument();
  });
});
