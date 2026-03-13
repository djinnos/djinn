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
    it("shows loading text while checking authentication", () => {
      // fetchState won't resolve — keeps loading
      mockInvoke.mockReturnValue(new Promise(() => {}));

      renderAuthGate();

      expect(screen.getByText("Checking authentication...")).toBeInTheDocument();
      expect(screen.queryByTestId("protected")).not.toBeInTheDocument();
    });

    it("does not show sign-in screen while loading", () => {
      mockInvoke.mockReturnValue(new Promise(() => {}));

      renderAuthGate();

      expect(screen.queryByText("Sign in required")).not.toBeInTheDocument();
    });
  });

  describe("unauthenticated state", () => {
    it("shows sign-in screen when not authenticated", async () => {
      mockInvoke.mockResolvedValueOnce({ isAuthenticated: false, user: null });

      renderAuthGate();

      await waitFor(() => {
        expect(screen.getByText("Sign in required")).toBeInTheDocument();
      });
    });

    it("shows default message when no error", async () => {
      mockInvoke.mockResolvedValueOnce({ isAuthenticated: false, user: null });

      renderAuthGate();

      await waitFor(() => {
        expect(screen.getByText("Please sign in to continue to Djinn.")).toBeInTheDocument();
      });
    });

    it("shows error message when error exists", async () => {
      mockInvoke.mockRejectedValueOnce(new Error("Connection refused"));

      renderAuthGate();

      await waitFor(() => {
        expect(screen.getByText(/Connection refused/)).toBeInTheDocument();
      });
    });

    it("does not render children when unauthenticated", async () => {
      mockInvoke.mockResolvedValueOnce({ isAuthenticated: false, user: null });

      renderAuthGate();

      await waitFor(() => {
        expect(screen.getByText("Sign in required")).toBeInTheDocument();
      });
      expect(screen.queryByTestId("protected")).not.toBeInTheDocument();
    });

    it("shows sign-in button", async () => {
      mockInvoke.mockResolvedValueOnce({ isAuthenticated: false, user: null });

      renderAuthGate();

      await waitFor(() => {
        expect(screen.getByRole("button", { name: "Sign in" })).toBeInTheDocument();
      });
    });

    it("calls login when sign-in button is clicked", async () => {
      mockInvoke
        .mockResolvedValueOnce({ isAuthenticated: false, user: null }) // fetchState
        .mockResolvedValueOnce(undefined); // login

      const user = userEvent.setup();
      renderAuthGate();

      await waitFor(() => {
        expect(screen.getByRole("button", { name: "Sign in" })).toBeInTheDocument();
      });

      await user.click(screen.getByRole("button", { name: "Sign in" }));

      expect(mockInvoke).toHaveBeenCalledWith("auth_login");
    });
  });

  describe("authenticated state", () => {
    it("renders children when authenticated", async () => {
      mockInvoke.mockResolvedValueOnce({
        isAuthenticated: true,
        user: MOCK_USER,
      });

      renderAuthGate();

      await waitFor(() => {
        expect(screen.getByTestId("protected")).toBeInTheDocument();
      });
    });

    it("does not show sign-in screen when authenticated", async () => {
      mockInvoke.mockResolvedValueOnce({
        isAuthenticated: true,
        user: MOCK_USER,
      });

      renderAuthGate();

      await waitFor(() => {
        expect(screen.getByTestId("protected")).toBeInTheDocument();
      });
      expect(screen.queryByText("Sign in required")).not.toBeInTheDocument();
    });

    it("does not show loading text when authenticated", async () => {
      mockInvoke.mockResolvedValueOnce({
        isAuthenticated: true,
        user: MOCK_USER,
      });

      renderAuthGate();

      await waitFor(() => {
        expect(screen.getByTestId("protected")).toBeInTheDocument();
      });
      expect(screen.queryByText("Checking authentication...")).not.toBeInTheDocument();
    });
  });

  describe("Tauri event handling", () => {
    it("updates state on auth:state-changed event", async () => {
      mockInvoke.mockResolvedValueOnce({ isAuthenticated: false, user: null });

      renderAuthGate();

      await waitFor(() => {
        expect(screen.getByText("Sign in required")).toBeInTheDocument();
      });

      // Simulate Tauri emitting auth state change
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

    it("handles auth:callback-received by exchanging code", async () => {
      const MOCK_CONFIG = { clientId: "test_client", redirectUri: "http://localhost:19876/auth/callback" };
      mockInvoke
        .mockResolvedValueOnce({ isAuthenticated: false, user: null }) // fetchState
        .mockResolvedValueOnce(MOCK_CONFIG) // getOAuthConfig
        .mockResolvedValueOnce(MOCK_USER); // exchangeAuthCode

      renderAuthGate();

      await waitFor(() => {
        expect(screen.getByText("Sign in required")).toBeInTheDocument();
      });

      act(() => {
        emitTauriEvent("auth:callback-received", {
          code: "auth_code_123",
          state: "random_state",
          code_verifier: "verifier_456",
        });
      });

      await waitFor(() => {
        expect(mockInvoke).toHaveBeenCalledWith("exchange_auth_code", {
          code: "auth_code_123",
          codeVerifier: "verifier_456",
          redirectUri: MOCK_CONFIG.redirectUri,
          clientId: MOCK_CONFIG.clientId,
        });
      });
    });

    it("sets error on auth:callback-received exchange failure", async () => {
      mockInvoke
        .mockResolvedValueOnce({ isAuthenticated: false, user: null }) // fetchState
        .mockResolvedValueOnce({ clientId: "c", redirectUri: "r" }) // getOAuthConfig
        .mockRejectedValueOnce(new Error("invalid_grant")); // exchangeAuthCode

      const consoleSpy = vi.spyOn(console, "error").mockImplementation(() => {});
      renderAuthGate();

      await waitFor(() => {
        expect(screen.getByText("Sign in required")).toBeInTheDocument();
      });

      act(() => {
        emitTauriEvent("auth:callback-received", {
          code: "bad_code",
          state: "state",
          code_verifier: "verifier",
        });
      });

      await waitFor(() => {
        expect(useAuthStore.getState().error).toContain("Authentication failed");
      });
      consoleSpy.mockRestore();
    });

    it("refetches state on auth:silent-refresh-success", async () => {
      mockInvoke
        .mockResolvedValueOnce({ isAuthenticated: false, user: null }) // initial fetchState
        .mockResolvedValueOnce({ isAuthenticated: true, user: MOCK_USER }); // refetch after refresh

      renderAuthGate();

      await waitFor(() => {
        expect(screen.getByText("Sign in required")).toBeInTheDocument();
      });

      act(() => {
        emitTauriEvent("auth:silent-refresh-success", {});
      });

      await waitFor(() => {
        expect(screen.getByTestId("protected")).toBeInTheDocument();
      });
    });

    it("shows sign-in on auth:login-required", async () => {
      // Start in loading state (fetchState never resolves)
      mockInvoke.mockReturnValue(new Promise(() => {}));

      renderAuthGate();

      expect(screen.getByText("Checking authentication...")).toBeInTheDocument();

      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

      await waitFor(() => {
        expect(screen.getByText("Sign in required")).toBeInTheDocument();
      });
    });

    it("shows sign-in on auth:silent-refresh-failed", async () => {
      mockInvoke.mockReturnValue(new Promise(() => {}));

      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:silent-refresh-failed", { reason: "token_expired" });
      });

      await waitFor(() => {
        expect(screen.getByText("Sign in required")).toBeInTheDocument();
      });
    });

    it("transitions from authenticated to unauthenticated on state-changed", async () => {
      mockInvoke.mockResolvedValueOnce({
        isAuthenticated: true,
        user: MOCK_USER,
      });

      renderAuthGate();

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
      mockInvoke.mockResolvedValueOnce({ isAuthenticated: true, user: MOCK_USER });

      const { unmount } = renderAuthGate();

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
      mockInvoke.mockResolvedValueOnce({
        isAuthenticated: true,
        user: MOCK_USER,
      });

      renderWithProviders(
        <AuthGate>
          <div data-testid="level-1">
            <div data-testid="level-2">
              <span>Deep nested content</span>
            </div>
          </div>
        </AuthGate>,
      );

      await waitFor(() => {
        expect(screen.getByTestId("level-1")).toBeInTheDocument();
        expect(screen.getByTestId("level-2")).toBeInTheDocument();
        expect(screen.getByText("Deep nested content")).toBeInTheDocument();
      });
    });

    it("renders multiple children", async () => {
      mockInvoke.mockResolvedValueOnce({
        isAuthenticated: true,
        user: MOCK_USER,
      });

      renderWithProviders(
        <AuthGate>
          <div data-testid="child-1">First</div>
          <div data-testid="child-2">Second</div>
        </AuthGate>,
      );

      await waitFor(() => {
        expect(screen.getByTestId("child-1")).toBeInTheDocument();
        expect(screen.getByTestId("child-2")).toBeInTheDocument();
      });
    });
  });
});
