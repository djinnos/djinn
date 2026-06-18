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
    fetchLearnedPromptHistory: vi.fn(),
    createAgent: vi.fn(),
    updateAgent: vi.fn(),
    deleteAgent: vi.fn(),
    clearLearnedPrompt: vi.fn(),
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
  verification_command: null,
  is_default: false,
  learned_prompt: null,
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

  it("loads and renders default and specialist role content", async () => {
    vi.mocked(fetchAgents).mockResolvedValue([defaultWorker, reviewerSpecialist, architectDefault]);

    render(<AgentRoles />);

    expect(screen.getByText("Loading roles...")).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "Agent Roles" })).toBeInTheDocument();

    expect(fetchAgents).toHaveBeenCalledWith("project-1");
    expect(screen.getByText("Handles implementation tasks")).toBeInTheDocument();
    expect(screen.getByText("Designs system architecture")).toBeInTheDocument();
    expect(screen.getAllByText("default").length).toBe(2);
    expect(screen.getByText("Strict Reviewer")).toBeInTheDocument();
    expect(screen.getByText("Reviews risky backend changes")).toBeInTheDocument();
    expect(screen.getByText("1 ext")).toBeInTheDocument();
    expect(screen.getByText("2 exts")).toBeInTheDocument();
    expect(screen.getAllByText("Worker")[0]).toBeInTheDocument();
    expect(screen.getByText("Reviewer")).toBeInTheDocument();
    expect(screen.getByText("Architect")).toBeInTheDocument();
    expect(screen.getByText("2 MCP")).toBeInTheDocument();
    expect(screen.getByText("2 skills")).toBeInTheDocument();
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
      verification_command: "pnpm test AgentRoles.test.tsx",
    });

    vi.mocked(fetchAgents).mockResolvedValue([]);
    vi.mocked(createAgent).mockResolvedValue(created);

    render(<AgentRoles />);

    await user.click(await screen.findByRole("button", { name: "New Specialist" }));

    expect(await screen.findByRole("heading", { name: "New specialist" })).toBeInTheDocument();
    expect(fetchAvailableMcpServers).toHaveBeenCalledWith("project-1");
    expect(fetchAvailableSkills).toHaveBeenCalledWith("project-1");

    await user.click(screen.getByRole("button", { name: "Task Reviewer" }));
    await user.type(screen.getByLabelText("Name"), "Frontend Reviewer");
    await user.type(screen.getByLabelText("Description"), "Reviews React UI changes");
    await user.type(screen.getByLabelText("System prompt extensions"), "Prefer accessible queries.");
    await user.type(screen.getByLabelText("Verification command"), "pnpm test AgentRoles.test.tsx");

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
        verification_command: "pnpm test AgentRoles.test.tsx",
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
      verification_command: "pnpm test -- AgentRoles",
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
    await user.clear(screen.getByLabelText("Verification command"));
    await user.type(screen.getByLabelText("Verification command"), "pnpm test -- AgentRoles");

    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => {
      expect(updateAgent).toHaveBeenCalledWith("reviewer-specialist", {
        name: "Careful Reviewer",
        description: "Reviews high-risk backend changes",
        system_prompt_extensions: ["Check migrations.", "Check concurrency."],
        mcp_servers: ["github", "postgres"],
        skills: ["rust-review", "testing-library"],
        verification_command: "pnpm test -- AgentRoles",
      });
    });

    expect(await screen.findByText("Careful Reviewer")).toBeInTheDocument();
    expect(screen.getByText("Reviews high-risk backend changes")).toBeInTheDocument();
  });

  it("renders an error state when the initial role load fails", async () => {
    vi.mocked(fetchAgents).mockRejectedValue(new Error("agents exploded"));

    render(<AgentRoles />);

    expect(await screen.findByText("agents exploded")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Agent Roles" })).toBeInTheDocument();
  });
});
