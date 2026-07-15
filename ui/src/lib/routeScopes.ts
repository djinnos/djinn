/**
 * Route-scope classification — determines which filtering / context
 * mechanism applies on every page so that chrome controls are only
 * rendered when they actually affect the page's data.
 *
 * Scopes
 * ------
 *  **global-project-context**
 *    The page reads `useProjectStore` (selectedProjectId / useSelectedProject).
 *    A single shared ProjectSelector in the application chrome writes that
 *    store.  Routes: `/agents`, `/task/:taskId`, `/memory`, `/code-graph`.
 *
 *  **url-filtered**
 *    The page keeps its own filter state in URL search-params (query-string).
 *    No global project selector is shown because the URL *is* the source of
 *    truth for multi-select or cross-project filters.
 *    Routes: `/tasks`, `/dependencies` (board multi-select via URL params),
 *    `/admin/usage` (usage filters via URL params — see below).
 *
 *  **path-scoped**
 *    The page's project identity comes from a URL path segment rather than
 *    the global store.  No global selector is needed.
 *    Routes: `/projects/:id/environment`.
 *
 *  **global**
 *    The page operates across all projects or has no project concept.
 *    No global selector.
 *    Routes: `/proposals`, `/images`, `/repositories`, `/users`,
 *    `/settings`, `/chat`, `/projects` list.
 */

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type RouteScope =
  | "global-project-context"
  | "url-filtered"
  | "path-scoped"
  | "global";

/**
 * A route entry pairs a path pattern with its scope.
 * Patterns use the same syntax as react-router `<Route path>`.
 */
export interface RouteScopeEntry {
  /** react-router-compatible path pattern (may include :params and * wildcards) */
  pattern: string;
  scope: RouteScope;
  /**
   * global-project-context pages that render their OWN writer of the global
   * project store (e.g. the galaxy HUD project chip) — the shared chrome
   * ProjectSelector is suppressed for them so the control isn't duplicated.
   */
  inPageSelector?: boolean;
  /** Human-readable note — purely documentary */
  note?: string;
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/**
 * Canonical route-scope registry.
 *
 * When adding a new route, pick the narrowest correct scope.  If the page
 * reads `useSelectedProject()` / `useSelectedProjectId()` from the global
 * store, it belongs in **global-project-context**.  If it persists its own
 * filters in URL search-params, use **url-filtered**.  If the project is
 * part of the URL path, use **path-scoped**.  Otherwise **global**.
 */
export const ROUTE_SCOPES: readonly RouteScopeEntry[] = [
  // ── global-project-context ──────────────────────────────────────────────
  {
    pattern: "/agents",
    scope: "global-project-context",
    note: "AgentsPage reads useSelectedProject() for metrics/MCP context",
  },
  {
    pattern: "/task/:taskId",
    scope: "global-project-context",
    note: "TaskSessionPage reads useSelectedProject() for session context/back-nav",
  },
  {
    pattern: "/code-graph",
    scope: "global-project-context",
    inPageSelector: true,
    note: "CodeGraphPage reads useSelectedProject(); the galaxy HUD project chip writes the store, so the chrome selector is suppressed",
  },
  {
    pattern: "/memory",
    scope: "global-project-context",
    note: "MemoryPage reads useSelectedProject() for memory_list/read/search; local picker removed",
  },

  // ── url-filtered ────────────────────────────────────────────────────────
  {
    pattern: "/tasks",
    scope: "url-filtered",
    note: "Board multi-select filters live in URL search-params",
  },
  {
    pattern: "/dependencies",
    scope: "url-filtered",
    note: "Dependencies board filters live in URL search-params",
  },
  {
    pattern: "/admin/usage",
    scope: "url-filtered",
    note:
      "UsageDashboard filters live in URL search-params. " +
      "Supported URL keys: preset, start, end, granularity, project_id, " +
      "model, agent_type, user_id. " +
      "Note: project_id is a dashboard reporting filter scoped to the " +
      "usage analytics query — it is NOT the global project context " +
      "from useProjectStore. The global ProjectSelector is hidden on " +
      "this route.",
  },

  // ── path-scoped ─────────────────────────────────────────────────────────
  {
    pattern: "/projects/:id/environment",
    scope: "path-scoped",
    note: "Project identity is in the URL path segment",
  },

  // ── global (cross-project or no project concept) ────────────────────────
  { pattern: "/proposals", scope: "global", note: "Cross-project list" },
  { pattern: "/proposals/:id", scope: "global" },
  { pattern: "/images", scope: "global" },
  { pattern: "/images/*", scope: "global" },
  { pattern: "/repositories", scope: "global" },
  { pattern: "/users", scope: "global" },
  { pattern: "/settings", scope: "global" },
  { pattern: "/settings/*", scope: "global" },
  { pattern: "/chat", scope: "global" },
  { pattern: "/chat/:sessionId", scope: "global" },
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Return the scope entry for the given pathname, or `undefined` when the
 * path matches no registered pattern (which means the catch-all redirect).
 */
export function getRouteScopeEntry(
  pathname: string,
): RouteScopeEntry | undefined {
  return ROUTE_SCOPES.find((entry) => matchPath(entry.pattern, pathname));
}

/** Convenience: does this pathname read the global project store? */
export function isGlobalProjectContextRoute(pathname: string): boolean {
  return getRouteScopeEntry(pathname)?.scope === "global-project-context";
}

/**
 * Should the shared chrome ProjectSelector render for this pathname?
 * True for global-project-context routes that do NOT carry their own
 * in-page writer of the project store.
 */
export function needsChromeProjectSelector(pathname: string): boolean {
  const entry = getRouteScopeEntry(pathname);
  return entry?.scope === "global-project-context" && !entry.inPageSelector;
}

/**
 * Minimal path matcher that mirrors react-router's `<Route path>` semantics
 * for simple patterns:
 *   - literal segments: `/agents`
 *   - param segments:   `:taskId`
 *   - wildcard trailing: `*` (matches everything after)
 *
 * This is intentionally simple — it covers the patterns we register above.
 * If more complex matching is needed, switch to `matchPath` from
 * react-router-dom.
 */
function matchPath(pattern: string, pathname: string): boolean {
  const patternParts = pattern.split("/").filter(Boolean);
  const pathParts = pathname.split("/").filter(Boolean);

  for (let i = 0; i < patternParts.length; i++) {
    const pp = patternParts[i];

    // Wildcard — matches everything remaining
    if (pp === "*") return true;

    // Need a corresponding path segment
    if (i >= pathParts.length) return false;

    // Param segment — matches any non-empty value
    if (pp.startsWith(":")) continue;

    // Literal — must match exactly
    if (pp !== pathParts[i]) return false;
  }

  // All pattern segments consumed — lengths must match (unless wildcard handled above)
  return patternParts.length === pathParts.length;
}
