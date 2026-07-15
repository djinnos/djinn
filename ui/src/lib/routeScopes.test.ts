import { describe, it, expect } from "vitest";
import {
  getRouteScopeEntry,
  isGlobalProjectContextRoute,
  needsChromeProjectSelector,
  ROUTE_SCOPES,
} from "./routeScopes";

describe("routeScopes", () => {
  describe("isGlobalProjectContextRoute", () => {
    it("returns true for /agents", () => {
      expect(isGlobalProjectContextRoute("/agents")).toBe(true);
    });

    it("returns true for /task/:taskId (any task id)", () => {
      expect(isGlobalProjectContextRoute("/task/abc-123")).toBe(true);
      expect(isGlobalProjectContextRoute("/task/xyz")).toBe(true);
    });

    it("returns true for /memory (local picker removed)", () => {
      expect(isGlobalProjectContextRoute("/memory")).toBe(true);
    });

    it("returns true for /code-graph (reads the global store)", () => {
      expect(isGlobalProjectContextRoute("/code-graph")).toBe(true);
    });

    it("returns false for /tasks (board — url-filtered)", () => {
      expect(isGlobalProjectContextRoute("/tasks")).toBe(false);
    });

    it("returns false for /dependencies", () => {
      expect(isGlobalProjectContextRoute("/dependencies")).toBe(false);
    });

    it("returns false for /admin/usage", () => {
      expect(isGlobalProjectContextRoute("/admin/usage")).toBe(false);
    });

    it("returns false for /proposals", () => {
      expect(isGlobalProjectContextRoute("/proposals")).toBe(false);
    });

    it("returns false for /images", () => {
      expect(isGlobalProjectContextRoute("/images")).toBe(false);
    });

    it("returns false for /repositories", () => {
      expect(isGlobalProjectContextRoute("/repositories")).toBe(false);
    });

    it("returns false for /users", () => {
      expect(isGlobalProjectContextRoute("/users")).toBe(false);
    });

    it("returns false for /settings", () => {
      expect(isGlobalProjectContextRoute("/settings")).toBe(false);
    });

    it("returns false for /chat", () => {
      expect(isGlobalProjectContextRoute("/chat")).toBe(false);
    });

    it("returns false for /projects/:id/environment (path-scoped)", () => {
      expect(isGlobalProjectContextRoute("/projects/proj-1/environment")).toBe(
        false,
      );
    });
  });

  describe("needsChromeProjectSelector", () => {
    it("returns true for global-project-context routes without an in-page selector", () => {
      expect(needsChromeProjectSelector("/agents")).toBe(true);
      expect(needsChromeProjectSelector("/task/abc-123")).toBe(true);
      expect(needsChromeProjectSelector("/memory")).toBe(true);
    });

    it("returns false for /code-graph (galaxy HUD project chip writes the store)", () => {
      expect(needsChromeProjectSelector("/code-graph")).toBe(false);
    });

    it("returns false for non-global-project-context routes", () => {
      expect(needsChromeProjectSelector("/tasks")).toBe(false);
      expect(needsChromeProjectSelector("/proposals")).toBe(false);
    });
  });

  describe("getRouteScopeEntry", () => {
    it("returns global-project-context scope for /agents", () => {
      const entry = getRouteScopeEntry("/agents");
      expect(entry).toBeDefined();
      expect(entry!.scope).toBe("global-project-context");
    });

    it("returns url-filtered scope for /tasks", () => {
      const entry = getRouteScopeEntry("/tasks");
      expect(entry).toBeDefined();
      expect(entry!.scope).toBe("url-filtered");
    });

    it("returns url-filtered scope for /admin/usage", () => {
      const entry = getRouteScopeEntry("/admin/usage");
      expect(entry).toBeDefined();
      expect(entry!.scope).toBe("url-filtered");
    });

    it("returns path-scoped for /projects/:id/environment", () => {
      const entry = getRouteScopeEntry("/projects/proj-1/environment");
      expect(entry).toBeDefined();
      expect(entry!.scope).toBe("path-scoped");
    });

    it("returns global scope for /chat/:sessionId", () => {
      const entry = getRouteScopeEntry("/chat/session-123");
      expect(entry).toBeDefined();
      expect(entry!.scope).toBe("global");
    });

    it("returns global-project-context scope for /memory", () => {
      const entry = getRouteScopeEntry("/memory");
      expect(entry).toBeDefined();
      expect(entry!.scope).toBe("global-project-context");
    });

    it("returns global-project-context scope for /code-graph", () => {
      const entry = getRouteScopeEntry("/code-graph");
      expect(entry).toBeDefined();
      expect(entry!.scope).toBe("global-project-context");
    });

    it("returns undefined for unregistered paths", () => {
      expect(getRouteScopeEntry("/unknown")).toBeUndefined();
    });
  });

  describe("ROUTE_SCOPES registry", () => {
    it("covers all listed cross-project/global routes", () => {
      const globalRoutes = [
        "/tasks",
        "/dependencies",
        "/admin/usage",
        "/proposals",
        "/images",
        "/repositories",
        "/users",
        "/settings",
        "/chat",
        "/projects/proj-1/environment",
      ];

      for (const path of globalRoutes) {
        expect(isGlobalProjectContextRoute(path)).toBe(false);
      }
    });

    it("every registered route has a known scope", () => {
      const validScopes = new Set([
        "global-project-context",
        "url-filtered",
        "path-scoped",
        "global",
      ]);
      for (const entry of ROUTE_SCOPES) {
        expect(validScopes.has(entry.scope)).toBe(true);
      }
    });

    it("has no duplicate pattern entries", () => {
      const patterns = ROUTE_SCOPES.map((e) => e.pattern);
      const unique = new Set(patterns);
      expect(unique.size).toBe(patterns.length);
    });

    it("/memory appears exactly once with global-project-context scope", () => {
      const memoryEntries = ROUTE_SCOPES.filter((e) => e.pattern === "/memory");
      expect(memoryEntries).toHaveLength(1);
      expect(memoryEntries[0].scope).toBe("global-project-context");
    });

    it("/code-graph appears exactly once with global-project-context scope", () => {
      const entries = ROUTE_SCOPES.filter((e) => e.pattern === "/code-graph");
      expect(entries).toHaveLength(1);
      expect(entries[0].scope).toBe("global-project-context");
    });

    it("/admin/usage note documents all URL keys and project_id distinction", () => {
      const entry = ROUTE_SCOPES.find((e) => e.pattern === "/admin/usage");
      expect(entry).toBeDefined();
      const note = entry!.note ?? "";
      // Every supported URL key must be documented.
      const requiredKeys = [
        "preset",
        "start",
        "end",
        "granularity",
        "project_id",
        "model",
        "agent_type",
        "user_id",
      ];
      for (const key of requiredKeys) {
        expect(note).toContain(key);
      }
      // Clarify project_id is not global project context.
      expect(note).toContain("NOT the global project context");
    });
  });
});
