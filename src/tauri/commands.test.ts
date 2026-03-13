import { invoke } from "@tauri-apps/api/core";
import {
  getServerPort,
  getServerStatus,
  retryServerDiscovery,
  selectDirectory,
  checkGitRemote,
  setupGitRemote,
  authGetState,
  authLogin,
  authLogout,
  exchangeAuthCode,
  getOAuthConfig,
  CLERK_DOMAIN,
} from "./commands";

const mockInvoke = vi.mocked(invoke);

describe("tauri/commands", () => {
  beforeEach(() => {
    mockInvoke.mockReset();
  });

  describe("constants", () => {
    it("exports correct CLERK_DOMAIN", () => {
      expect(CLERK_DOMAIN).toBe("clerk.djinnai.io");
    });
  });

  describe("getOAuthConfig", () => {
    it("invokes get_oauth_config command", async () => {
      const config = { clientId: "test_id", redirectUri: "http://localhost/callback" };
      mockInvoke.mockResolvedValueOnce(config);

      const result = await getOAuthConfig();

      expect(mockInvoke).toHaveBeenCalledWith("get_oauth_config");
      expect(result).toEqual(config);
    });
  });

  describe("getServerPort", () => {
    it("invokes get_server_port command", async () => {
      mockInvoke.mockResolvedValueOnce(8080);

      const result = await getServerPort();

      expect(mockInvoke).toHaveBeenCalledWith("get_server_port");
      expect(result).toBe(8080);
    });
  });

  describe("getServerStatus", () => {
    it("invokes get_server_status command", async () => {
      const status = {
        port: 8080,
        is_healthy: true,
        has_error: false,
        error_message: null,
      };
      mockInvoke.mockResolvedValueOnce(status);

      const result = await getServerStatus();

      expect(mockInvoke).toHaveBeenCalledWith("get_server_status");
      expect(result).toEqual(status);
    });

    it("returns error state correctly", async () => {
      const status = {
        port: null,
        is_healthy: false,
        has_error: true,
        error_message: "Server crashed",
      };
      mockInvoke.mockResolvedValueOnce(status);

      const result = await getServerStatus();

      expect(result.has_error).toBe(true);
      expect(result.error_message).toBe("Server crashed");
      expect(result.port).toBeNull();
    });
  });

  describe("retryServerDiscovery", () => {
    it("invokes retry_server_discovery command", async () => {
      mockInvoke.mockResolvedValueOnce(9090);

      const result = await retryServerDiscovery();

      expect(mockInvoke).toHaveBeenCalledWith("retry_server_discovery");
      expect(result).toBe(9090);
    });
  });

  describe("selectDirectory", () => {
    it("invokes select_directory with title", async () => {
      mockInvoke.mockResolvedValueOnce("/home/user/project");

      const result = await selectDirectory("Choose folder");

      expect(mockInvoke).toHaveBeenCalledWith("select_directory", {
        title: "Choose folder",
      });
      expect(result).toBe("/home/user/project");
    });

    it("invokes select_directory without title", async () => {
      mockInvoke.mockResolvedValueOnce(null);

      const result = await selectDirectory();

      expect(mockInvoke).toHaveBeenCalledWith("select_directory", {
        title: undefined,
      });
      expect(result).toBeNull();
    });

    it("returns null when user cancels", async () => {
      mockInvoke.mockResolvedValueOnce(null);

      const result = await selectDirectory("Pick");

      expect(result).toBeNull();
    });
  });

  describe("checkGitRemote", () => {
    it("returns remote URL when configured", async () => {
      mockInvoke.mockResolvedValueOnce("git@github.com:user/repo.git");

      const result = await checkGitRemote("/path/to/repo");

      expect(mockInvoke).toHaveBeenCalledWith("check_git_remote", {
        projectPath: "/path/to/repo",
      });
      expect(result).toBe("git@github.com:user/repo.git");
    });

    it("returns null when no remote", async () => {
      mockInvoke.mockResolvedValueOnce(null);

      const result = await checkGitRemote("/path/to/repo");

      expect(result).toBeNull();
    });
  });

  describe("setupGitRemote", () => {
    it("sets up remote and returns success message", async () => {
      mockInvoke.mockResolvedValueOnce("Remote added and pushed");

      const result = await setupGitRemote(
        "/path/to/repo",
        "git@github.com:user/repo.git",
      );

      expect(mockInvoke).toHaveBeenCalledWith("setup_git_remote", {
        projectPath: "/path/to/repo",
        remoteUrl: "git@github.com:user/repo.git",
      });
      expect(result).toBe("Remote added and pushed");
    });
  });

  describe("authGetState", () => {
    it("invokes auth_get_state and returns auth state", async () => {
      const state = {
        isAuthenticated: true,
        user: { sub: "user_1", name: "User", email: "u@test.com" },
      };
      mockInvoke.mockResolvedValueOnce(state);

      const result = await authGetState();

      expect(mockInvoke).toHaveBeenCalledWith("auth_get_state");
      expect(result).toEqual(state);
    });

    it("returns unauthenticated state", async () => {
      mockInvoke.mockResolvedValueOnce({
        isAuthenticated: false,
        user: null,
      });

      const result = await authGetState();

      expect(result.isAuthenticated).toBe(false);
      expect(result.user).toBeNull();
    });
  });

  describe("authLogin", () => {
    it("invokes auth_login", async () => {
      mockInvoke.mockResolvedValueOnce(undefined);

      await authLogin();

      expect(mockInvoke).toHaveBeenCalledWith("auth_login");
    });

    it("propagates errors", async () => {
      mockInvoke.mockRejectedValueOnce(new Error("failed"));

      await expect(authLogin()).rejects.toThrow("failed");
    });
  });

  describe("authLogout", () => {
    it("invokes auth_logout", async () => {
      mockInvoke.mockResolvedValueOnce(undefined);

      await authLogout();

      expect(mockInvoke).toHaveBeenCalledWith("auth_logout");
    });

    it("propagates errors", async () => {
      mockInvoke.mockRejectedValueOnce(new Error("revoke failed"));

      await expect(authLogout()).rejects.toThrow("revoke failed");
    });
  });

  describe("exchangeAuthCode", () => {
    it("invokes exchange_auth_code with correct params", async () => {
      const user = { sub: "user_1", name: "User" };
      mockInvoke.mockResolvedValueOnce(user);

      const result = await exchangeAuthCode(
        "code_abc",
        "verifier_xyz",
        "djinn://auth/callback",
        "rXf6AlZNrHOcJ2HV",
      );

      expect(mockInvoke).toHaveBeenCalledWith("exchange_auth_code", {
        code: "code_abc",
        codeVerifier: "verifier_xyz",
        redirectUri: "djinn://auth/callback",
        clientId: "rXf6AlZNrHOcJ2HV",
      });
      expect(result).toEqual(user);
    });

    it("propagates exchange errors", async () => {
      mockInvoke.mockRejectedValueOnce(new Error("invalid_grant"));

      await expect(
        exchangeAuthCode("bad", "bad", "uri", "client"),
      ).rejects.toThrow("invalid_grant");
    });
  });
});
