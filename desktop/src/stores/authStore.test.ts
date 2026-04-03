import { useAuthStore } from "./authStore";
import type { AuthState } from "@/electron/commands";

const mockInvoke = window.electronAPI.invoke as ReturnType<typeof vi.fn>;

const MOCK_USER = {
  sub: "user_123",
  name: "Test User",
  email: "test@example.com",
  picture: "https://example.com/avatar.png",
};

const AUTHENTICATED_STATE: AuthState = {
  isAuthenticated: true,
  user: MOCK_USER,
};

const UNAUTHENTICATED_STATE: AuthState = {
  isAuthenticated: false,
  user: null,
};

function resetStore() {
  useAuthStore.setState({
    isAuthenticated: false,
    user: null,
    isLoading: true,
    error: null,
  });
}

describe("authStore", () => {
  beforeEach(() => {
    resetStore();
    mockInvoke.mockReset();
  });

  describe("initial state", () => {
    it("starts with isLoading true", () => {
      expect(useAuthStore.getState().isLoading).toBe(true);
    });

    it("starts unauthenticated", () => {
      expect(useAuthStore.getState().isAuthenticated).toBe(false);
    });

    it("starts with no user", () => {
      expect(useAuthStore.getState().user).toBeNull();
    });

    it("starts with no error", () => {
      expect(useAuthStore.getState().error).toBeNull();
    });
  });

  describe("fetchState", () => {
    it("fetches authenticated state from backend", async () => {
      mockInvoke.mockResolvedValueOnce(AUTHENTICATED_STATE);

      await useAuthStore.getState().fetchState();

      expect(mockInvoke).toHaveBeenCalledWith("auth_get_state");
      expect(useAuthStore.getState().isAuthenticated).toBe(true);
      expect(useAuthStore.getState().user).toEqual(MOCK_USER);
      expect(useAuthStore.getState().isLoading).toBe(false);
      expect(useAuthStore.getState().error).toBeNull();
    });

    it("fetches unauthenticated state from backend", async () => {
      mockInvoke.mockResolvedValueOnce(UNAUTHENTICATED_STATE);

      await useAuthStore.getState().fetchState();

      expect(useAuthStore.getState().isAuthenticated).toBe(false);
      expect(useAuthStore.getState().user).toBeNull();
      expect(useAuthStore.getState().isLoading).toBe(false);
    });

    it("handles fetch error gracefully", async () => {
      mockInvoke.mockRejectedValueOnce(new Error("Backend unavailable"));

      await useAuthStore.getState().fetchState();

      expect(useAuthStore.getState().isLoading).toBe(false);
      expect(useAuthStore.getState().error).toBe("Error: Backend unavailable");
      expect(useAuthStore.getState().isAuthenticated).toBe(false);
    });

    it("clears previous error on successful fetch", async () => {
      useAuthStore.setState({ error: "previous error" });
      mockInvoke.mockResolvedValueOnce(AUTHENTICATED_STATE);

      await useAuthStore.getState().fetchState();

      expect(useAuthStore.getState().error).toBeNull();
    });

    it("preserves user with partial profile (no picture)", async () => {
      const userNoPicture = { sub: "user_456", name: "No Pic", email: "nopic@test.com" };
      mockInvoke.mockResolvedValueOnce({
        isAuthenticated: true,
        user: userNoPicture,
      });

      await useAuthStore.getState().fetchState();

      expect(useAuthStore.getState().user).toEqual(userNoPicture);
    });

    it("preserves user with minimal profile (sub only)", async () => {
      const minimalUser = { sub: "user_789" };
      mockInvoke.mockResolvedValueOnce({
        isAuthenticated: true,
        user: minimalUser,
      });

      await useAuthStore.getState().fetchState();

      expect(useAuthStore.getState().user).toEqual(minimalUser);
      expect(useAuthStore.getState().isAuthenticated).toBe(true);
    });
  });

  describe("setState", () => {
    it("updates to authenticated state", () => {
      useAuthStore.getState().setState(AUTHENTICATED_STATE);

      expect(useAuthStore.getState().isAuthenticated).toBe(true);
      expect(useAuthStore.getState().user).toEqual(MOCK_USER);
      expect(useAuthStore.getState().isLoading).toBe(false);
      expect(useAuthStore.getState().error).toBeNull();
    });

    it("updates to unauthenticated state", () => {
      useAuthStore.setState({ isAuthenticated: true, user: MOCK_USER });

      useAuthStore.getState().setState(UNAUTHENTICATED_STATE);

      expect(useAuthStore.getState().isAuthenticated).toBe(false);
      expect(useAuthStore.getState().user).toBeNull();
    });

    it("clears error when setting state", () => {
      useAuthStore.setState({ error: "some error" });

      useAuthStore.getState().setState(AUTHENTICATED_STATE);

      expect(useAuthStore.getState().error).toBeNull();
    });

    it("sets isLoading to false", () => {
      expect(useAuthStore.getState().isLoading).toBe(true);

      useAuthStore.getState().setState(UNAUTHENTICATED_STATE);

      expect(useAuthStore.getState().isLoading).toBe(false);
    });
  });

  describe("login", () => {
    it("calls auth_login command", async () => {
      mockInvoke.mockResolvedValueOnce(undefined);

      await useAuthStore.getState().login();

      expect(mockInvoke).toHaveBeenCalledWith("auth_login");
    });

    it("clears error before login attempt", async () => {
      useAuthStore.setState({ error: "previous error" });
      mockInvoke.mockResolvedValueOnce(undefined);

      await useAuthStore.getState().login();

      expect(useAuthStore.getState().error).toBeNull();
    });

    it("sets error on login failure", async () => {
      mockInvoke.mockRejectedValueOnce(new Error("Browser launch failed"));

      await useAuthStore.getState().login();

      expect(useAuthStore.getState().error).toBe("Error: Browser launch failed");
    });

    it("does not change auth state on login call (state changes come via events)", async () => {
      mockInvoke.mockResolvedValueOnce(undefined);

      await useAuthStore.getState().login();

      // login() just opens browser — auth state doesn't change until callback
      expect(useAuthStore.getState().isAuthenticated).toBe(false);
    });
  });

  describe("logout", () => {
    it("calls auth_logout command and clears state", async () => {
      useAuthStore.setState({ isAuthenticated: true, user: MOCK_USER });
      mockInvoke.mockResolvedValueOnce(undefined);

      await useAuthStore.getState().logout();

      expect(mockInvoke).toHaveBeenCalledWith("auth_logout");
      expect(useAuthStore.getState().isAuthenticated).toBe(false);
      expect(useAuthStore.getState().user).toBeNull();
      expect(useAuthStore.getState().error).toBeNull();
    });

    it("sets error on logout failure", async () => {
      useAuthStore.setState({ isAuthenticated: true, user: MOCK_USER });
      mockInvoke.mockRejectedValueOnce(new Error("Revocation failed"));

      await useAuthStore.getState().logout();

      expect(useAuthStore.getState().error).toBe("Error: Revocation failed");
    });

    it("does not clear auth state on logout failure", async () => {
      useAuthStore.setState({ isAuthenticated: true, user: MOCK_USER });
      mockInvoke.mockRejectedValueOnce(new Error("Network error"));

      await useAuthStore.getState().logout();

      // On error, state is not explicitly cleared — only error is set
      expect(useAuthStore.getState().error).toBe("Error: Network error");
    });
  });

  describe("concurrent operations", () => {
    it("handles rapid fetchState calls", async () => {
      const firstState: AuthState = { isAuthenticated: false, user: null };
      const secondState: AuthState = { isAuthenticated: true, user: MOCK_USER };

      mockInvoke
        .mockResolvedValueOnce(firstState)
        .mockResolvedValueOnce(secondState);

      await Promise.all([
        useAuthStore.getState().fetchState(),
        useAuthStore.getState().fetchState(),
      ]);

      // Last write wins — both should have completed
      expect(mockInvoke).toHaveBeenCalledTimes(2);
      expect(useAuthStore.getState().isLoading).toBe(false);
    });

    it("handles login during fetchState", async () => {
      mockInvoke
        .mockResolvedValueOnce(UNAUTHENTICATED_STATE) // fetchState
        .mockResolvedValueOnce(undefined); // login

      await Promise.all([
        useAuthStore.getState().fetchState(),
        useAuthStore.getState().login(),
      ]);

      expect(mockInvoke).toHaveBeenCalledTimes(2);
    });
  });
});
