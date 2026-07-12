import { beforeEach, describe, expect, it, vi } from "vitest";

import { callMcpTool } from "@/api/mcpClient";
import { getServerBaseUrl } from "@/api/serverUrl";
import {
  checkServerHealth,
  addProjectFromGithub,
  fetchProviderCatalog,
  getGithubInstallUrl,
  githubRepoToProjectArgs,
  invalidateProviderCatalogCache,
  listGithubRepos,
  searchTasksAcrossProjects,
  SEARCH_RESULT_CAP,
  startProviderOAuth,
} from "@/api/server";
import { projectStore } from "@/stores/projectStore";
import { taskStore } from "@/stores/taskStore";
import { mergedColumnStore } from "@/stores/mergedColumnStore";

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: vi.fn(),
}));

vi.mock("@/api/serverUrl", () => ({
  getServerBaseUrl: vi.fn(),
}));

const callMcpToolMock = vi.mocked(callMcpTool);
const getServerBaseUrlMock = vi.mocked(getServerBaseUrl);

function providerCatalogResponse(providers: unknown[]) {
  return {
    providers,
    total: providers.length,
  };
}

function providerCatalogItem(overrides: Record<string, unknown>) {
  return {
    base_url: "https://api.example.test",
    builtin_id: "builtin-provider",
    connected: false,
    connection_methods: [],
    docs_url: "https://docs.example.test",
    env_vars: [],
    goose_provider_id: "goose-provider",
    id: "provider",
    is_openai_compatible: false,
    name: "Provider",
    npm: "@example/provider",
    oauth_keys: [],
    oauth_supported: false,
    ...overrides,
  };
}

beforeEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  callMcpToolMock.mockReset();
  getServerBaseUrlMock.mockReset();
  getServerBaseUrlMock.mockReturnValue("https://djinn.example.test");
  invalidateProviderCatalogCache();
});

describe("server API helpers", () => {
  describe("checkServerHealth", () => {
    it("fetches the health endpoint from the configured base URL", async () => {
      const json = vi.fn().mockResolvedValue({ status: "ok" });
      const fetchMock = vi.fn().mockResolvedValue({ ok: true, json });
      vi.stubGlobal("fetch", fetchMock);

      await expect(checkServerHealth()).resolves.toEqual({ status: "ok" });

      expect(getServerBaseUrlMock).toHaveBeenCalledTimes(1);
      expect(fetchMock).toHaveBeenCalledWith("https://djinn.example.test/health");
      expect(json).toHaveBeenCalledTimes(1);
    });

    it("throws when the health endpoint returns a non-ok response", async () => {
      const json = vi.fn();
      vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 503, json }));

      await expect(checkServerHealth()).rejects.toThrow("Health check failed: 503");
      expect(json).not.toHaveBeenCalled();
    });
  });

  describe("fetchProviderCatalog", () => {
    it("normalizes catalog providers and caches the MCP response", async () => {
      callMcpToolMock.mockResolvedValueOnce(
        providerCatalogResponse([
          providerCatalogItem({
            id: "openai",
            name: "OpenAI",
            env_vars: ["OPENAI_API_KEY"],
            oauth_supported: true,
            connection_methods: ["oauth", "api_key"],
            revoked_reason: "token revoked",
          }),
          providerCatalogItem({
            id: "local",
            name: "Local",
            env_vars: [],
            oauth_supported: false,
            connection_methods: [],
          }),
        ]) as never,
      );

      await expect(fetchProviderCatalog()).resolves.toEqual([
        {
          id: "openai",
          name: "OpenAI",
          description: "OAuth supported",
          requires_api_key: true,
          oauth_supported: true,
          connection_methods: ["oauth", "api_key"],
          revoked_reason: "token revoked",
        },
        {
          id: "local",
          name: "Local",
          description: "",
          requires_api_key: false,
          oauth_supported: false,
          connection_methods: [],
          revoked_reason: undefined,
        },
      ]);
      await fetchProviderCatalog();

      expect(callMcpToolMock).toHaveBeenCalledTimes(1);
      expect(callMcpToolMock).toHaveBeenCalledWith("provider_catalog");
    });

    it("refetches the provider catalog after invalidating the cache", async () => {
      callMcpToolMock
        .mockResolvedValueOnce(
          providerCatalogResponse([
            providerCatalogItem({ id: "first", name: "First provider" }),
          ]) as never,
        )
        .mockResolvedValueOnce(
          providerCatalogResponse([
            providerCatalogItem({ id: "second", name: "Second provider" }),
          ]) as never,
        );

      await expect(fetchProviderCatalog()).resolves.toMatchObject([{ id: "first" }]);
      invalidateProviderCatalogCache();
      await expect(fetchProviderCatalog()).resolves.toMatchObject([{ id: "second" }]);

      expect(callMcpToolMock).toHaveBeenCalledTimes(2);
      expect(callMcpToolMock).toHaveBeenNthCalledWith(1, "provider_catalog");
      expect(callMcpToolMock).toHaveBeenNthCalledWith(2, "provider_catalog");
    });
  });

  describe("startProviderOAuth", () => {
    it("starts OAuth with the provider id and reports immediate success", async () => {
      callMcpToolMock.mockResolvedValueOnce({ ok: true, pending: false } as never);

      await expect(startProviderOAuth("openai")).resolves.toEqual({ success: true });
      expect(callMcpToolMock).toHaveBeenCalledWith("provider_oauth_start", {
        provider_id: "openai",
      });
    });

    it("returns the server failure error when OAuth start is not ok", async () => {
      callMcpToolMock.mockResolvedValueOnce({ ok: false, error: "OAuth disabled" } as never);

      await expect(startProviderOAuth("openai")).resolves.toEqual({
        success: false,
        error: "OAuth disabled",
      });
    });

    it("returns a default failure error when OAuth start fails without a message", async () => {
      callMcpToolMock.mockResolvedValueOnce({ ok: false } as never);

      await expect(startProviderOAuth("openai")).resolves.toEqual({
        success: false,
        error: "OAuth flow failed",
      });
    });

    it("normalizes pending device-code OAuth responses", async () => {
      callMcpToolMock.mockResolvedValueOnce({
        ok: true,
        pending: true,
        user_code: "ABCD-1234",
        verification_uri: "https://auth.example.test/device",
        verification_uri_complete: "https://auth.example.test/device?user_code=ABCD-1234",
        interval: 5,
        expires_in: 900,
      } as never);

      await expect(startProviderOAuth("openai")).resolves.toEqual({
        success: false,
        pending: true,
        user_code: "ABCD-1234",
        verification_uri: "https://auth.example.test/device",
        verification_uri_complete: "https://auth.example.test/device?user_code=ABCD-1234",
        interval: 5,
        expires_in: 900,
      });
    });

    it("returns a failure result when the MCP call throws", async () => {
      callMcpToolMock.mockRejectedValueOnce(new Error("MCP unavailable"));

      await expect(startProviderOAuth("openai")).resolves.toEqual({
        success: false,
        error: "MCP unavailable",
      });
    });
  });

  describe("searchTasksAcrossProjects", () => {
    beforeEach(() => {
      taskStore.getState().clearTasks();
      mergedColumnStore.getState().reset();
      projectStore.getState().setProjects([
        {
          id: "proj-1",
          name: "One",
          github_owner: "acme",
          github_repo: "one",
        },
        {
          id: "proj-2",
          name: "Two",
          github_owner: "acme",
          github_repo: "two",
        },
      ] as never);
    });

    it("queries every project with the case-insensitive text filter and upserts matches", async () => {
      callMcpToolMock.mockImplementation((async (_tool: string, params: { project: string }) => {
        if (params.project === "acme/one") {
          return { tasks: [{ id: "t-1", title: "Auth login", status: "closed" }] };
        }
        return { tasks: [{ id: "t-2", title: "AUTHZ policy", status: "closed" }] };
      }) as never);

      const result = await searchTasksAcrossProjects(["acme/one", "acme/two"], "  auth  ");

      expect(result).toHaveLength(2);
      // Each project queried once with the trimmed text and the result cap.
      expect(callMcpToolMock).toHaveBeenCalledWith("task_list", {
        project: "acme/one",
        text: "auth",
        limit: SEARCH_RESULT_CAP,
        offset: 0,
      });
      // Matches are stamped with their project id and merged into the store.
      expect(taskStore.getState().getTask("t-1")?.project_id).toBe("proj-1");
      expect(taskStore.getState().getTask("t-2")?.project_id).toBe("proj-2");
      // The Merged-column pagination cursors are left untouched by search.
      expect(mergedColumnStore.getState().hasMore()).toBe(false);
      expect(Object.keys(mergedColumnStore.getState().projects)).toHaveLength(0);
    });

    it("short-circuits on an empty query without calling the backend", async () => {
      await expect(searchTasksAcrossProjects(["acme/one"], "   ")).resolves.toEqual([]);
      expect(callMcpToolMock).not.toHaveBeenCalled();
    });
  });

  describe("GitHub project helpers", () => {
    it("preserves a repository's non-main default branch when adding it", async () => {
      callMcpToolMock.mockResolvedValueOnce({
        status: "ok",
        project: { id: "project-1", github_owner: "acme", github_repo: "legacy" },
      } as never);
      const args = githubRepoToProjectArgs({
        owner: "acme",
        repo: "legacy",
        default_branch: "master",
        private: true,
        installation_id: 42,
        account_login: "acme",
      });

      await addProjectFromGithub(args);

      expect(callMcpToolMock).toHaveBeenCalledWith("project_add_from_github", {
        owner: "acme",
        repo: "legacy",
        ref: "master",
        installation_id: 42,
      });
    });

    it("throws the normalized error message from github_list_repos", async () => {
      callMcpToolMock.mockResolvedValueOnce({ status: "error: sign in required" } as never);

      await expect(listGithubRepos()).rejects.toThrow("sign in required");
      expect(callMcpToolMock).toHaveBeenCalledWith("github_list_repos", { per_page: 50 });
    });

    it("throws when the GitHub install URL output is malformed", async () => {
      callMcpToolMock.mockResolvedValueOnce({ status: "ok" } as never);

      await expect(getGithubInstallUrl()).rejects.toThrow(
        "GitHub App install URL is not configured on the server",
      );
      expect(callMcpToolMock).toHaveBeenCalledWith("github_app_install_url", {});
    });
  });
});
