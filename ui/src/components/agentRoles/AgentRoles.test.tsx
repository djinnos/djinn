import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, userEvent, waitFor } from "@/test/test-utils";
import { AgentRoles } from "@/components/AgentRoles";
import * as agentsApi from "@/api/agents";
import { projectStore } from "@/stores/projectStore";

vi.mock("@/api/agents", async (importOriginal) => {
  const actual = await importOriginal<typeof agentsApi>();
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

// Provide an admin auth user so the "New Specialist" button is rendered
// and the AgentCard edit/delete controls are visible.
vi.mock("@/components/AuthGate", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/components/AuthGate")>();
  return {
    ...actual,
    useAuthUser: () => ({
      id: "admin-1",
      login: "admin",
      name: "Admin",
      avatarUrl: null,
      isAdmin: true,
    }),
  };
});

const mockRole = (overrides: Partial<agentsApi.Agent> = {}): agentsApi.Agent => ({
  id: "role-1",
  name: "Senior Backend Worker",
  base_role: "worker",
  description: "Owns backend services",
  system_prompt_extensions: ["Always write safe code"],
  mcp_servers: ["github"],
  skills: ["code-review"],
  model_preference: null,
  is_default: false,
  ...overrides,
});

describe("AgentRoles shell", () => {
  beforeEach(() => {
    vi.mocked(agentsApi.fetchAgents).mockReset();
    vi.mocked(agentsApi.fetchAvailableMcpServers).mockReset();
    vi.mocked(agentsApi.fetchAvailableSkills).mockReset();
    vi.mocked(agentsApi.deleteAgent).mockReset();

    vi.mocked(agentsApi.fetchAvailableMcpServers).mockResolvedValue([]);
    vi.mocked(agentsApi.fetchAvailableSkills).mockResolvedValue([]);

    projectStore.setState({
      projects: [
        { id: "project-1", name: "Project One", github_owner: "djinn", github_repo: "one" },
      ],
      selectedProjectId: "project-1",
      lastViewPerProject: {},
    });
  });

  it("renders the agent roles shell and the new-specialist control for admins", async () => {
    vi.mocked(agentsApi.fetchAgents).mockResolvedValue([mockRole()]);

    render(<AgentRoles />);

    expect(await screen.findByRole("heading", { name: "Agent Roles" })).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /New Specialist/i }),
    ).toBeInTheDocument();
    // Role card surfaces the role's name and base-role label
    expect(screen.getByText("Senior Backend Worker")).toBeInTheDocument();
    expect(screen.getByText("Worker")).toBeInTheDocument();
  });

  it("shows the empty state when no roles are returned", async () => {
    vi.mocked(agentsApi.fetchAgents).mockResolvedValue([]);

    render(<AgentRoles />);

    expect(
      await screen.findByText(/No project-default agents are available yet/i),
    ).toBeInTheDocument();
    expect(screen.getByText(/No specialists configured yet/i)).toBeInTheDocument();
  });

  it("invokes the delete mutation when an admin confirms deletion", async () => {
    const user = userEvent.setup();
    const role = mockRole({ id: "role-2", name: "Temp Helper" });
    vi.mocked(agentsApi.fetchAgents).mockResolvedValue([role]);
    vi.mocked(agentsApi.deleteAgent).mockResolvedValue(undefined);

    render(<AgentRoles />);

    const card = await screen.findByText("Temp Helper");
    const cardContainer = card.closest(".group") as HTMLElement;
    expect(cardContainer).not.toBeNull();

    // Hover to make the admin-only action buttons visible
    await user.hover(cardContainer!);

    // The action group is the only div with two button children
    // (Edit + the ConfirmButton trigger). The ConfirmButton trigger
    // wraps an svg icon and is the second button in that group.
    const actionButtons = cardContainer!.querySelectorAll("button");
    expect(actionButtons.length).toBeGreaterThanOrEqual(2);
    const deleteTrigger = actionButtons[1] as HTMLElement;
    await user.click(deleteTrigger);

    // The ConfirmButton opens an alert dialog — confirm it
    const confirmButton = await screen.findByRole("button", { name: /^Delete$/i });
    await user.click(confirmButton);

    await waitFor(() => {
      expect(agentsApi.deleteAgent).toHaveBeenCalledWith("role-2");
    });
  });
});
