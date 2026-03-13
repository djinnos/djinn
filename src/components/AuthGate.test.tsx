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
        expect(screen.getByRole("button", { name: "Sign in" })).toBeInTheDocument();
      });
    });

    it("calls login when sign-in button is clicked", async () => {
      mockInvoke.mockResolvedValueOnce(undefined); // login

      const user = userEvent.setup();
      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

      await waitFor(() => {
        expect(screen.getByRole("button", { name: "Sign in" })).toBeInTheDocument();
      });

      await user.click(screen.getByRole("button", { name: "Sign in" }));

      expect(mockInvoke).toHaveBeenCalledWith("auth_login");
    });
  });

  describe("callback handling", () => {
    it("handles auth:callback-received by exchanging code", async () => {
      const MOCK_CONFIG = { clientId: "test_client", redirectUri: "http://localhost:19876/auth/callback" };
      mockInvoke
        .mockResolvedValueOnce(MOCK_CONFIG) // getOAuthConfig
        .mockResolvedValueOnce(MOCK_USER); // exchangeAuthCode

      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

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
        .mockResolvedValueOnce({ clientId: "c", redirectUri: "r" }) // getOAuthConfig
        .mockRejectedValueOnce(new Error("invalid_grant")); // exchangeAuthCode

      const consoleSpy = vi.spyOn(console, "error").mockImplementation(() => {});
      renderAuthGate();

      act(() => {
        emitTauriEvent("auth:login-required", {});
      });

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
