import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { useTaskStore } from "@/stores/useTaskStore";
import { useEpicStore } from "@/stores/useEpicStore";
import { useProjects } from "@/stores/useProjectStore";
import { callMcpTool } from "@/api/mcpClient";
import {
  matchesStructuredFilters,
  useBoardFilters,
} from "@/components/board/boardFilters";
import { toGraphData, type BlockerItem } from "@/components/graph/graph-adapter";
import DependencyGraph from "@/components/graph/DependencyGraph";

/**
 * Fetches blockers for a batch of (task_id, project_slug) pairs in parallel.
 * task_blockers_list requires a project parameter, so we look up each task's
 * project slug and call the tool with the right scope.
 */
async function fetchAllBlockers(
  pairs: Array<{ id: string; projectSlug: string }>,
): Promise<Map<string, BlockerItem[]>> {
  const result = new Map<string, BlockerItem[]>();
  if (pairs.length === 0) return result;

  const BATCH_SIZE = 20;
  for (let i = 0; i < pairs.length; i += BATCH_SIZE) {
    const batch = pairs.slice(i, i + BATCH_SIZE);
    const responses = await Promise.all(
      batch.map(async ({ id, projectSlug }) => {
        try {
          const response = await callMcpTool("task_blockers_list", {
            id,
            project: projectSlug,
          });
          return { id, blockers: (response.blockers ?? []) as BlockerItem[] };
        } catch {
          return { id, blockers: [] };
        }
      }),
    );

    for (const { id, blockers } of responses) {
      if (blockers.length > 0) {
        result.set(id, blockers);
      }
    }
  }

  return result;
}

export function DependenciesPage() {
  const tasks = useTaskStore((state) => Array.from(state.tasks.values()));
  const epics = useEpicStore((state) => state.epics);
  const projects = useProjects();
  const {
    projectFilters,
    epicFilters,
    ownerFilters,
    issueTypeFilters,
    search,
  } = useBoardFilters();

  // Map project_id → slug so we can route each task's blocker fetch to the
  // right project. (task_blockers_list still requires a project parameter.)
  const projectSlugById = useMemo(() => {
    const m = new Map<string, string>();
    for (const p of projects) {
      m.set(p.id, `${p.github_owner}/${p.github_repo}`);
    }
    return m;
  }, [projects]);

  const blockerPairs = useMemo(() => {
    const pairs: Array<{ id: string; projectSlug: string }> = [];
    for (const t of tasks) {
      if (t.issue_type === "epic") continue;
      const slug = t.project_id ? projectSlugById.get(t.project_id) : undefined;
      if (!slug) continue;
      pairs.push({ id: t.id, projectSlug: slug });
    }
    return pairs;
  }, [tasks, projectSlugById]);

  const blockerPairsKey = useMemo(
    () => blockerPairs.map((p) => `${p.id}@${p.projectSlug}`).join(","),
    [blockerPairs],
  );

  // Fetch blockers for all tasks — cached and refreshed with tasks
  const { data: blockersByTask, isLoading: blockersLoading } = useQuery({
    queryKey: ["roadmap-blockers", blockerPairsKey],
    queryFn: () => fetchAllBlockers(blockerPairs),
    enabled: blockerPairs.length > 0,
    staleTime: 30_000, // 30s — blockers change infrequently
    placeholderData: (prev) => prev,
  });

  const blockers = useMemo(
    () => blockersByTask ?? new Map<string, BlockerItem[]>(),
    [blockersByTask],
  );

  // Apply the structured filters, but keep dependency chains intact: tasks
  // that match the filters are "in scope", and any (transitive) blocker of an
  // in-scope task is pulled in too — rendered faded so out-of-scope context
  // isn't lost. The free-text search HIGHLIGHTS rather than hides.
  const { graphData, dimTaskIds, highlightTaskIds } = useMemo(() => {
    const structured = {
      projectFilters,
      epicFilters,
      ownerFilters,
      issueTypeFilters,
    };
    const hasStructured =
      projectFilters.length > 0 ||
      epicFilters.length > 0 ||
      ownerFilters.length > 0 ||
      issueTypeFilters.length > 0;

    const inScope = new Set<string>();
    for (const t of tasks) {
      if (t.issue_type === "epic") continue;
      if (matchesStructuredFilters(t, structured)) inScope.add(t.id);
    }

    // Walk blockers upstream to keep chains whole; mark pulled-in (out-of-
    // scope) tasks for fading.
    const included = new Set(inScope);
    const dim = new Set<string>();
    const queue = [...inScope];
    while (queue.length > 0) {
      const id = queue.pop()!;
      for (const b of blockers.get(id) ?? []) {
        const bid = b.blocking_task_id;
        if (!bid || included.has(bid)) continue;
        included.add(bid);
        if (!inScope.has(bid)) dim.add(bid);
        queue.push(bid);
      }
    }

    const scopedTasks = hasStructured
      ? tasks.filter((t) => included.has(t.id))
      : tasks;

    // Search → highlight matches, fade everything else for contrast.
    const q = search.trim().toLowerCase();
    const highlight = new Set<string>();
    if (q) {
      for (const t of scopedTasks) {
        if (t.issue_type === "epic") continue;
        if (t.title.toLowerCase().includes(q)) highlight.add(t.id);
        else dim.add(t.id);
      }
    }

    return {
      graphData: toGraphData(scopedTasks, epics, blockers),
      dimTaskIds: dim,
      highlightTaskIds: highlight,
    };
  }, [
    tasks,
    epics,
    blockers,
    projectFilters,
    epicFilters,
    ownerFilters,
    issueTypeFilters,
    search,
  ]);

  const hasData = graphData.some((g) => g.tasks.length > 0);

  if (blockersLoading) {
    return (
      <div className="flex h-full items-center justify-center">
        <p className="text-sm text-muted-foreground">Loading dependencies…</p>
      </div>
    );
  }

  if (!hasData) {
    return (
      <div className="flex h-full items-center justify-center px-6 text-center">
        <p className="text-sm text-muted-foreground">
          No tasks match the current filters. Clear a filter or create tasks
          with dependencies to see the graph.
        </p>
      </div>
    );
  }

  return (
    <div className="h-full w-full">
      <DependencyGraph
        epics={graphData}
        dimTaskIds={dimTaskIds}
        highlightTaskIds={highlightTaskIds}
      />
    </div>
  );
}
