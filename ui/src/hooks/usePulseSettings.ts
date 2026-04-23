import { useCallback, useMemo } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { callMcpTool } from "@/api/mcpClient";

/**
 * Pulse calibration settings — per-project exclusion globs plus the
 * Dead-code panel's orphan-ignore list. Backed by Dolt via the
 * `project_config_{get,set}` MCP tools (columns `graph_excluded_paths`
 * and `graph_orphan_ignore`, added in migration 12). The same lists
 * are consumed by the server-side `code_graph` handler, so any CLI /
 * MCP caller — not just the UI — sees the same filtered result set.
 *
 * The hook preserves its pre-migration synchronous surface: consumers
 * read `.settings.excluded_paths` / `.orphan_ignore` without awaiting
 * anything, and while the query is in flight the lists render as
 * empty (matches the "no exclusions, show everything" behaviour a new
 * project starts with).
 */

export interface PulseSettings {
  excluded_paths: string[];
  orphan_ignore: string[];
}

const EMPTY: PulseSettings = { excluded_paths: [], orphan_ignore: [] };

function queryKey(projectPath: string | null) {
  return ["pulse", "settings", projectPath] as const;
}

async function fetchSettings(projectPath: string): Promise<PulseSettings> {
  const response = await callMcpTool("project_config_get", {
    project: projectPath,
  });
  // The server returns `status: "ok"` on success and `"error: ..."`
  // on failure. On error we fall through to empty rather than
  // throwing, so the settings editor stays mountable for fresh
  // projects and transient lookup failures.
  return {
    excluded_paths: response.graph_excluded_paths ?? [],
    orphan_ignore: response.graph_orphan_ignore ?? [],
  };
}

async function writeList(
  projectPath: string,
  key: "graph_excluded_paths" | "graph_orphan_ignore",
  items: string[],
): Promise<PulseSettings> {
  const response = await callMcpTool("project_config_set", {
    project: projectPath,
    key,
    value: JSON.stringify(items),
  });
  if (!response.status.startsWith("ok")) {
    throw new Error(response.status);
  }
  return {
    excluded_paths: response.graph_excluded_paths ?? [],
    orphan_ignore: response.graph_orphan_ignore ?? [],
  };
}

export function usePulseSettings(projectPath: string | null) {
  const queryClient = useQueryClient();

  const query = useQuery({
    queryKey: queryKey(projectPath),
    queryFn: () => fetchSettings(projectPath!),
    enabled: !!projectPath,
    staleTime: 60_000,
  });

  const settings: PulseSettings = query.data ?? EMPTY;

  const mutation = useMutation({
    mutationFn: async (args: {
      key: "graph_excluded_paths" | "graph_orphan_ignore";
      items: string[];
    }) => {
      if (!projectPath) {
        throw new Error("no project path");
      }
      return writeList(projectPath, args.key, args.items);
    },
    onMutate: async (args) => {
      // Optimistic update: write the new list into the cache before
      // the server round-trips, so the list editor reflects the edit
      // on the next paint. The server's canonicalization (trim /
      // dedup) overwrites this in onSuccess — good enough for the
      // typical path where the user's input is already clean.
      if (!projectPath) return;
      await queryClient.cancelQueries({ queryKey: queryKey(projectPath) });
      const prev = queryClient.getQueryData<PulseSettings>(queryKey(projectPath));
      queryClient.setQueryData<PulseSettings>(queryKey(projectPath), {
        excluded_paths:
          args.key === "graph_excluded_paths"
            ? args.items
            : (prev?.excluded_paths ?? []),
        orphan_ignore:
          args.key === "graph_orphan_ignore"
            ? args.items
            : (prev?.orphan_ignore ?? []),
      });
      return { prev };
    },
    onError: (_err, _args, ctx) => {
      // Roll back to whatever the cache held before onMutate wrote.
      if (!projectPath) return;
      if (ctx?.prev) {
        queryClient.setQueryData(queryKey(projectPath), ctx.prev);
      }
    },
    onSuccess: (data) => {
      if (!projectPath) return;
      queryClient.setQueryData(queryKey(projectPath), data);
    },
  });

  const addExcludedPath = useCallback(
    (pattern: string) => {
      const trimmed = pattern.trim();
      if (!trimmed || settings.excluded_paths.includes(trimmed)) return;
      mutation.mutate({
        key: "graph_excluded_paths",
        items: [...settings.excluded_paths, trimmed],
      });
    },
    [mutation, settings.excluded_paths],
  );

  const removeExcludedPath = useCallback(
    (pattern: string) => {
      mutation.mutate({
        key: "graph_excluded_paths",
        items: settings.excluded_paths.filter((p) => p !== pattern),
      });
    },
    [mutation, settings.excluded_paths],
  );

  const addOrphanIgnore = useCallback(
    (path: string) => {
      const trimmed = path.trim();
      if (!trimmed || settings.orphan_ignore.includes(trimmed)) return;
      mutation.mutate({
        key: "graph_orphan_ignore",
        items: [...settings.orphan_ignore, trimmed],
      });
    },
    [mutation, settings.orphan_ignore],
  );

  const removeOrphanIgnore = useCallback(
    (path: string) => {
      mutation.mutate({
        key: "graph_orphan_ignore",
        items: settings.orphan_ignore.filter((p) => p !== path),
      });
    },
    [mutation, settings.orphan_ignore],
  );

  return useMemo(
    () => ({
      settings,
      addExcludedPath,
      removeExcludedPath,
      addOrphanIgnore,
      removeOrphanIgnore,
    }),
    [
      settings,
      addExcludedPath,
      removeExcludedPath,
      addOrphanIgnore,
      removeOrphanIgnore,
    ],
  );
}
