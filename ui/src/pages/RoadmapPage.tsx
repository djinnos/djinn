import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { useTaskStore } from "@/stores/useTaskStore";
import { useEpicStore } from "@/stores/useEpicStore";
import { useProjects } from "@/stores/useProjectStore";
import { callMcpTool } from "@/api/mcpClient";
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

export function RoadmapPage() {
  const tasks = useTaskStore((state) => Array.from(state.tasks.values()));
  const epics = useEpicStore((state) => state.epics);
  const projects = useProjects();

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

  const graphData = useMemo(
    () => toGraphData(tasks, epics, blockersByTask ?? new Map()),
    [tasks, epics, blockersByTask],
  );

  const hasData = graphData.some((g) => g.tasks.length > 0);

  if (blockersLoading) {
    return (
      <div className="flex h-full items-center justify-center">
        <div className="text-center">
          <p className="text-sm text-muted-foreground">Loading roadmap…</p>
        </div>
      </div>
    );
  }

  if (!hasData) {
    return (
      <div className="flex h-full items-center justify-center">
        <div className="text-center">
          <p className="text-sm text-muted-foreground">
            No tasks to display. Create tasks with dependencies to see the roadmap.
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="h-full w-full">
      <DependencyGraph epics={graphData} />
    </div>
  );
}
