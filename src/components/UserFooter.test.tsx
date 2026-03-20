/**
 * Tests for the UserFooter rendered inside the Sidebar.
 *
 * Since UserFooter is not exported, we test it indirectly through the Sidebar
 * component. All Sidebar dependencies are mocked to isolate the auth behavior.
 */
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { invoke } from "@tauri-apps/api/core";
import { useAuthStore } from "@/stores/authStore";
import { useSidebarStore } from "@/stores/sidebarStore";
import { renderWithProviders } from "@/test/helpers";
import { Sidebar } from "./Sidebar";

const mockInvoke = vi.mocked(invoke);

// Mock heavy Sidebar dependencies that are unrelated to auth
vi.mock("@/hooks/useExecutionStatus", () => ({
  useExecutionStatus: () => ({ state: "idle", refresh: vi.fn() }),
}));
vi.mock("@/hooks/useExecutionControl", () => ({
  useExecutionControl: () => ({
    start: vi.fn(),
    pause: vi.fn(),
    resume: vi.fn(),
  }),
}));
vi.mock("@/hooks/useProjectRoute", () => ({
  useProjectRoute: () => ({
    navigateToProject: vi.fn(),
    navigateToView: vi.fn(),
  }),
}));
vi.mock("@/stores/useProjectStore", () => ({
  useProjects: () => [],
  useSelectedProjectId: () => "__all__",
}));
vi.mock("@/stores/projectStore", () => ({
  ALL_PROJECTS: "__all__",
}));
// Mock hugeicons to avoid import issues
vi.mock("@hugeicons/core-free-icons", () => ({
  KanbanIcon: "KanbanIcon",
  Robot01Icon: "Robot01Icon",
  ChartHistogramIcon: "ChartHistogramIcon",
  ChatIcon: "ChatIcon",
  ArrowDown01Icon: "ArrowDown01Icon",
  Folder02Icon: "Folder02Icon",
  PlusSignIcon: "PlusSignIcon",
  LogoutSquare01Icon: "LogoutSquare01Icon",
}));
vi.mock("@hugeicons/react", () => ({
  HugeiconsIcon: ({ children }: { children?: React.ReactNode }) => <span>{children}</span>,
}));

const MOCK_USER = {
  sub: "user_123",
  name: "Alice Smith",
  email: "alice@example.com",
  picture: "https://example.com/alice.png",
};

function setAuthState(overrides: Partial<ReturnType<typeof useAuthStore.getState>>) {
  useAuthStore.setState({
    isAuthenticated: true,
    user: MOCK_USER,
    isLoading: false,
    error: null,
    ...overrides,
  });
}

describe("UserFooter (via Sidebar)", () => {
  beforeEach(() => {
    useAuthStore.setState({
      isAuthenticated: false,
      user: null,
      isLoading: false,
      error: null,
    });
    mockInvoke.mockReset();
  });

  describe("when no user is present", () => {
    it("does not render user profile section", () => {
      renderWithProviders(<Sidebar />);

      expect(screen.queryByText("Sign out")).not.toBeInTheDocument();
      expect(screen.queryByTitle(/Sign out/)).not.toBeInTheDocument();
    });
  });

  describe("expanded sidebar with user", () => {
    beforeEach(() => {
      setAuthState({});
    });

    it("renders user name", () => {
      renderWithProviders(<Sidebar />);

      expect(screen.getByText("Alice Smith")).toBeInTheDocument();
    });

    it("renders user email", () => {
      renderWithProviders(<Sidebar />);

      expect(screen.getByText("alice@example.com")).toBeInTheDocument();
    });

    it("renders user avatar image when picture is provided", () => {
      renderWithProviders(<Sidebar />);

      const avatar = screen.getByAltText("");
      expect(avatar).toBeInTheDocument();
      expect(avatar).toHaveAttribute("src", "https://example.com/alice.png");
    });

    it("renders sign-out button with title", () => {
      renderWithProviders(<Sidebar />);

      expect(screen.getByTitle("Sign out")).toBeInTheDocument();
    });

    it("calls logout when sign-out button is clicked", async () => {
      mockInvoke.mockResolvedValueOnce(undefined); // auth_logout
      const user = userEvent.setup();

      renderWithProviders(<Sidebar />);

      await user.click(screen.getByTitle("Sign out"));

      expect(mockInvoke).toHaveBeenCalledWith("auth_logout");
    });

    it("renders fallback initial when no picture", () => {
      setAuthState({ user: { sub: "u1", name: "Bob", email: "bob@test.com" } });

      renderWithProviders(<Sidebar />);

      expect(screen.getByText("B")).toBeInTheDocument();
    });

    it("uses email initial when no name or picture", () => {
      setAuthState({ user: { sub: "u2", email: "zara@test.com" } });

      renderWithProviders(<Sidebar />);

      expect(screen.getByText("Z")).toBeInTheDocument();
    });

    it("uses ? when no name, email, or picture", () => {
      setAuthState({ user: { sub: "u3" } });

      renderWithProviders(<Sidebar />);

      expect(screen.getByText("?")).toBeInTheDocument();
    });

    it("displays 'User' as name fallback when name is missing", () => {
      setAuthState({ user: { sub: "u4", email: "test@test.com" } });

      renderWithProviders(<Sidebar />);

      expect(screen.getByText("User")).toBeInTheDocument();
    });

    it("does not render email line when email is missing", () => {
      setAuthState({ user: { sub: "u5", name: "No Email" } });

      renderWithProviders(<Sidebar />);

      expect(screen.getByText("No Email")).toBeInTheDocument();
      // Should not have an email paragraph — only the name paragraph
      const footer = screen.getByText("No Email").closest("div");
      const paragraphs = footer?.querySelectorAll("p");
      expect(paragraphs?.length).toBe(1);
    });
  });

  describe("collapsed sidebar with user", () => {
    beforeEach(() => {
      setAuthState({});
      useSidebarStore.setState({ isCollapsed: true });
    });

    afterEach(() => {
      useSidebarStore.setState({ isCollapsed: false });
    });

    it("renders avatar button with sign-out title", () => {
      renderWithProviders(<Sidebar />);

      const btn = screen.getByTitle("Alice Smith — Sign out");
      expect(btn).toBeInTheDocument();
    });

    it("renders avatar image in collapsed mode", () => {
      renderWithProviders(<Sidebar />);

      const avatar = screen.getByAltText("");
      expect(avatar).toHaveAttribute("src", "https://example.com/alice.png");
    });

    it("renders initial in collapsed mode when no picture", () => {
      setAuthState({ user: { sub: "u1", name: "Charlie" } });

      renderWithProviders(<Sidebar />);

      expect(screen.getByText("C")).toBeInTheDocument();
    });

    it("uses email for collapsed title when no name", () => {
      setAuthState({ user: { sub: "u1", email: "anon@test.com" } });

      renderWithProviders(<Sidebar />);

      expect(screen.getByTitle("anon@test.com — Sign out")).toBeInTheDocument();
    });

    it("falls back to 'User' in collapsed title", () => {
      setAuthState({ user: { sub: "u1" } });

      renderWithProviders(<Sidebar />);

      expect(screen.getByTitle("User — Sign out")).toBeInTheDocument();
    });

    it("calls logout when collapsed avatar is clicked", async () => {
      mockInvoke.mockResolvedValueOnce(undefined);
      const user = userEvent.setup();

      renderWithProviders(<Sidebar />);

      await user.click(screen.getByTitle("Alice Smith — Sign out"));

      expect(mockInvoke).toHaveBeenCalledWith("auth_logout");
    });
  });

  describe("auth state transitions", () => {
    it("shows user profile when auth state transitions to authenticated", async () => {
      renderWithProviders(<Sidebar />);

      expect(screen.queryByText("Alice Smith")).not.toBeInTheDocument();

      // Simulate auth state change
      setAuthState({});

      await waitFor(() => {
        expect(screen.getByText("Alice Smith")).toBeInTheDocument();
      });
    });

    it("hides user profile when auth state transitions to unauthenticated", async () => {
      setAuthState({});

      renderWithProviders(<Sidebar />);

      expect(screen.getByText("Alice Smith")).toBeInTheDocument();

      useAuthStore.setState({ isAuthenticated: false, user: null });

      await waitFor(() => {
        expect(screen.queryByText("Alice Smith")).not.toBeInTheDocument();
      });
    });
  });
});
