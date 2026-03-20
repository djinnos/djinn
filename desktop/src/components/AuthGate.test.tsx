import { screen, waitFor, act } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { invoke } from "@tauri-apps/api/core";
import { AuthGate } from "./AuthGate";
import { useAuthStore } from "@/stores/authStore";
import { renderWithProviders } from "@/test/helpers";
import { emitTauriEvent, clearTauriListeners } from "@/test/setup";

const mockInvoke = vi.mocked(invoke);

const MOCK_USER = {
  sub: "user_123",
  name: "Test User",
  email: "test@example.com",
  picture: "https://example.com/avatar.png",
};

function resetStore() {
  useAuthStore.setState({
    isAuthenticated: false,
    user: null,
    isLoading: true,
    error: null,
  });
}

function renderAuthGate(children = <div data-testid="protected">Protected content</div>) {
  return renderWithProviders(<AuthGate>{children}</AuthGate>);
}

describe("AuthGate", () => {
  beforeEach(() => {
    resetStore();
    clearTauriListeners();
    mockInvoke.mockReset();
  });

  describe("loading state", () => {
    it("shows loading text while waiting for backend events", () => {
      renderAuthGate();

      expect(screen.getByText("Checking authentication...")).toBeInTheDocument();
      expect(screen.queryByTestId("protected")).not.toBeInTheDocument();
    });

    it("does not call fetchState eagerly on mount", () => {
      renderAuthGate();

      // No invoke calls should happen on mount — state is event-driven
      expect(mockInvoke).not.toHaveBeenCalledWith("auth_get_state");
    });

    it("does not show sign-in screen while loading", () => {
      renderAuthGate();

      expect(screen.queryByText("Sign in required")).not.toBeInTheDocument();
    });
  });

  describe("fallback timer", () => {
    beforeEach(() => {
      vi.useFakeTimers();
    });

    afterEach(() => {
      vi.useRealTimers();
    });

    it("calls fetchState after 5s if still loading", async () => {
      mockInvoke.mockResolvedValueOnce({ isAuthenticated: false, user: null });

      renderAuthGate();

      // Before timer fires, no fetch
      expect(mockInvoke).not.toHaveBeenCalled();

      // Advance past 5s fallback and flush all pending promises
      await act(async () => {
        vi.advanceTimersByTime(5000);
      });

      expect(mockInvoke).toHaveBeenCalledWith("auth_get_state");
    }, 10000);

    it("does not call fetchState if event arrived before timeout", async () => {
      renderAuthGate();

      // Backend event arrives before timeout — sync state update
      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

      // Verify store updated (isLoading = false)
      expect(useAuthStore.getState().isLoading).toBe(false);

      // Advance past timeout — should not fetch since isLoading is now false
      await act(async () => {
        vi.advanceTimersByTime(5000);
      });

      expect(mockInvoke).not.toHaveBeenCalledWith("auth_get_state");
    }, 10000);
  });

  describe("event-driven auth state", () => {
    it("shows sign-in on auth:login-required event", async () => {
      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

      await waitFor(() => {
        expect(screen.getByText("Sign in required")).toBeInTheDocument();
      });
    });

    it("shows sign-in on auth:silent-refresh-failed event", async () => {
      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:silent-refresh-failed", { reason: "token_expired" });
      });

      await waitFor(() => {
        expect(screen.getByText("Sign in required")).toBeInTheDocument();
      });
    });

    it("shows children on auth:state-changed with authenticated state", async () => {
      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:state-changed", {
          isAuthenticated: true,
          user: MOCK_USER,
        });
      });

      await waitFor(() => {
        expect(screen.getByTestId("protected")).toBeInTheDocument();
      });
    });

    it("shows default message when no error", async () => {
      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

      await waitFor(() => {
        expect(screen.getByText("Please sign in to continue to Djinn.")).toBeInTheDocument();
      });
    });

    it("shows error message when error exists", async () => {
      useAuthStore.setState({ error: "Connection refused" });

      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

      await waitFor(() => {
        expect(screen.getByText("Connection refused")).toBeInTheDocument();
      });
    });

    it("does not render children when unauthenticated", async () => {
      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

      await waitFor(() => {
        expect(screen.getByText("Sign in required")).toBeInTheDocument();
      });
      expect(screen.queryByTestId("protected")).not.toBeInTheDocument();
    });
  });

  describe("sign-in button", () => {
    it("shows sign-in button when unauthenticated", async () => {
      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

      await waitFor(() => {
        expect(screen.getByRole("button", { name: "Sign in with GitHub" })).toBeInTheDocument();
      });
    });

    it("starts device flow when sign-in button is clicked", async () => {
      mockInvoke.mockResolvedValueOnce({
        userCode: "ABCD-1234",
        verificationUri: "https://github.com/login/device",
      });

      const user = userEvent.setup();
      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

      await waitFor(() => {
        expect(screen.getByRole("button", { name: "Sign in with GitHub" })).toBeInTheDocument();
      });

      await user.click(screen.getByRole("button", { name: "Sign in with GitHub" }));

      expect(mockInvoke).toHaveBeenCalledWith("start_github_login");

      // Should show device code UI
      await waitFor(() => {
        expect(screen.getByText("ABCD-1234")).toBeInTheDocument();
        expect(screen.getByText("Waiting for authorization...")).toBeInTheDocument();
      });
    });
  });

  describe("device code flow", () => {
    it("shows device code after starting login and transitions on auth:state-changed", async () => {
      mockInvoke.mockResolvedValueOnce({
        userCode: "WXYZ-5678",
        verificationUri: "https://github.com/login/device",
      });

      const user = userEvent.setup();
      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

      await waitFor(() => {
        expect(screen.getByRole("button", { name: "Sign in with GitHub" })).toBeInTheDocument();
      });

      await user.click(screen.getByRole("button", { name: "Sign in with GitHub" }));

      await waitFor(() => {
        expect(screen.getByText("WXYZ-5678")).toBeInTheDocument();
      });

      // Backend completes polling and emits auth:state-changed
      act(() => {
        emitTauriEvent("auth:state-changed", {
          isAuthenticated: true,
          user: MOCK_USER,
        });
      });

      await waitFor(() => {
        expect(screen.getByTestId("protected")).toBeInTheDocument();
      });
    });

    it("shows error on auth:login-failed", async () => {
      mockInvoke.mockResolvedValueOnce({
        userCode: "FAIL-0000",
        verificationUri: "https://github.com/login/device",
      });

      const user = userEvent.setup();
      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

      await waitFor(() => {
        expect(screen.getByRole("button", { name: "Sign in with GitHub" })).toBeInTheDocument();
      });

      await user.click(screen.getByRole("button", { name: "Sign in with GitHub" }));

      await waitFor(() => {
        expect(screen.getByText("FAIL-0000")).toBeInTheDocument();
      });

      act(() => {
        emitTauriEvent("auth:login-failed", { reason: "Device code expired" });
      });

      await waitFor(() => {
        // Should go back to sign-in state with error
        expect(screen.getByRole("button", { name: "Sign in with GitHub" })).toBeInTheDocument();
        expect(screen.getByText("Login failed: Device code expired")).toBeInTheDocument();
      });
    });
  });

  describe("silent refresh", () => {
    it("refetches state on auth:silent-refresh-success", async () => {
      mockInvoke.mockResolvedValueOnce({ isAuthenticated: true, user: MOCK_USER });

      renderAuthGate();

      // First transition to unauthenticated via event
      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

      await waitFor(() => {
        expect(screen.getByText("Sign in required")).toBeInTheDocument();
      });

      // Silent refresh succeeds — triggers fetchState
      act(() => {
        emitTauriEvent("auth:silent-refresh-success", {});
      });

      await waitFor(() => {
        expect(screen.getByTestId("protected")).toBeInTheDocument();
      });
    });
  });

  describe("state transitions", () => {
    it("transitions from authenticated to unauthenticated on state-changed", async () => {
      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:state-changed", {
          isAuthenticated: true,
          user: MOCK_USER,
        });
      });

      await waitFor(() => {
        expect(screen.getByTestId("protected")).toBeInTheDocument();
      });

      act(() => {
        emitTauriEvent("auth:state-changed", {
          isAuthenticated: false,
          user: null,
        });
      });

      await waitFor(() => {
        expect(screen.getByText("Sign in required")).toBeInTheDocument();
      });
    });
  });

  describe("cleanup", () => {
    it("unsubscribes event listeners on unmount", async () => {
      const { unmount } = renderAuthGate();

      act(() => {
        emitTauriEvent("auth:state-changed", {
          isAuthenticated: true,
          user: MOCK_USER,
        });
      });

      await waitFor(() => {
        expect(screen.getByTestId("protected")).toBeInTheDocument();
      });

      unmount();

      // Emitting after unmount should not cause errors
      act(() => {
        emitTauriEvent("auth:state-changed", {
          isAuthenticated: false,
          user: null,
        });
      });

      // No assertion needed — test passes if no errors thrown
    });
  });

  describe("children rendering", () => {
    it("renders complex children tree when authenticated", async () => {
      renderWithProviders(
        <AuthGate>
          <div data-testid="level-1">
            <div data-testid="level-2">
              <span>Deep nested content</span>
            </div>
          </div>
        </AuthGate>,
      );

      act(() => {
        emitTauriEvent("auth:state-changed", {
          isAuthenticated: true,
          user: MOCK_USER,
        });
      });

      await waitFor(() => {
        expect(screen.getByTestId("level-1")).toBeInTheDocument();
        expect(screen.getByTestId("level-2")).toBeInTheDocument();
        expect(screen.getByText("Deep nested content")).toBeInTheDocument();
      });
    });

    it("renders multiple children", async () => {
      renderWithProviders(
        <AuthGate>
          <div data-testid="child-1">First</div>
          <div data-testid="child-2">Second</div>
        </AuthGate>,
      );

      act(() => {
        emitTauriEvent("auth:state-changed", {
          isAuthenticated: true,
          user: MOCK_USER,
        });
      });

      await waitFor(() => {
        expect(screen.getByTestId("child-1")).toBeInTheDocument();
        expect(screen.getByTestId("child-2")).toBeInTheDocument();
      });
    });
  });
});
