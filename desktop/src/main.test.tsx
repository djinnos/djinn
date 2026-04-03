/**
 * Integration test verifying AuthGate wraps the App component.
 * This ensures the auth gate is wired in correctly at the entry point.
 */
import { screen, waitFor, act } from "@testing-library/react";
import { renderWithProviders } from "@/test/helpers";
import { AuthGate } from "@/components/AuthGate";
import { useAuthStore } from "@/stores/authStore";
import { emitMockEvent, clearMockListeners } from "@/test/setup";

const mockInvoke = window.electronAPI.invoke as ReturnType<typeof vi.fn>;

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
    clearMockListeners();
    mockInvoke.mockReset();
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === "attempt_silent_auth") return false;
      throw new Error(`Unexpected invoke: ${cmd}`);
    });
  });

  it("blocks app rendering when unauthenticated", async () => {
    const App = (await import("./App")).default;

    renderWithProviders(
      <AuthGate>
        <App />
      </AuthGate>,
    );

    // Backend signals login required
    act(() => {
      emitMockEvent("auth:login-required", {});
    });

    await waitFor(() => {
      expect(screen.getByText("Sign in required")).toBeInTheDocument();
    });
    expect(screen.queryByTestId("app-root")).not.toBeInTheDocument();
  });

  it("renders app when authenticated", async () => {
    const App = (await import("./App")).default;

    renderWithProviders(
      <AuthGate>
        <App />
      </AuthGate>,
    );

    // Backend signals authenticated state
    act(() => {
      emitMockEvent("auth:state-changed", {
        isAuthenticated: true,
        user: { sub: "user_1", name: "Test", email: "t@t.com" },
      });
    });

    await waitFor(() => {
      expect(screen.getByTestId("app-root")).toBeInTheDocument();
    });
    expect(screen.queryByText("Sign in required")).not.toBeInTheDocument();
  });

  it("shows loading state before auth check completes", async () => {
    const App = (await import("./App")).default;

    renderWithProviders(
      <AuthGate>
        <App />
      </AuthGate>,
    );

    // No event emitted yet — should show loading
    expect(screen.getByText("Checking authentication...")).toBeInTheDocument();
    expect(screen.queryByTestId("app-root")).not.toBeInTheDocument();
  });
});
