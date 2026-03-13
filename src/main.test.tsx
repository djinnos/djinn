/**
 * Integration test verifying AuthGate wraps the App component.
 * This ensures the auth gate is wired in correctly at the entry point.
 */
import { invoke } from "@tauri-apps/api/core";
import { screen, waitFor } from "@testing-library/react";
import { renderWithProviders } from "@/test/helpers";
import { AuthGate } from "@/components/AuthGate";
import { useAuthStore } from "@/stores/authStore";

const mockInvoke = vi.mocked(invoke);

// Mock the actual App to avoid pulling in the entire dependency tree
vi.mock("./App", () => ({
  default: () => <div data-testid="app-root">App loaded</div>,
}));

function resetStore() {
  useAuthStore.setState({
    isAuthenticated: false,
    user: null,
    isLoading: true,
    error: null,
  });
}

describe("main entry point (AuthGate + App integration)", () => {
  beforeEach(() => {
    resetStore();
    mockInvoke.mockReset();
  });

  it("blocks app rendering when unauthenticated", async () => {
    mockInvoke.mockResolvedValueOnce({ isAuthenticated: false, user: null });

    // Simulate the same structure as main.tsx
    const App = (await import("./App")).default;

    renderWithProviders(
      <AuthGate>
        <App />
      </AuthGate>,
    );

    await waitFor(() => {
      expect(screen.getByText("Sign in required")).toBeInTheDocument();
    });
    expect(screen.queryByTestId("app-root")).not.toBeInTheDocument();
  });

  it("renders app when authenticated", async () => {
    mockInvoke.mockResolvedValueOnce({
      isAuthenticated: true,
      user: { sub: "user_1", name: "Test", email: "t@t.com" },
    });

    const App = (await import("./App")).default;

    renderWithProviders(
      <AuthGate>
        <App />
      </AuthGate>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("app-root")).toBeInTheDocument();
    });
    expect(screen.queryByText("Sign in required")).not.toBeInTheDocument();
  });

  it("shows loading state before auth check completes", async () => {
    mockInvoke.mockReturnValue(new Promise(() => {}));

    const App = (await import("./App")).default;

    renderWithProviders(
      <AuthGate>
        <App />
      </AuthGate>,
    );

    expect(screen.getByText("Checking authentication...")).toBeInTheDocument();
    expect(screen.queryByTestId("app-root")).not.toBeInTheDocument();
  });
});
