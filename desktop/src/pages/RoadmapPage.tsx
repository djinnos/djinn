import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { useTaskStore } from "@/stores/useTaskStore";
import { useEpicStore } from "@/stores/useEpicStore";
import { useSelectedProjectId, useProjects } from "@/stores/useProjectStore";
import { callMcpTool } from "@/api/mcpClient";
import { toGraphData, type BlockerItem } from "@/components/graph/graph-adapter";
import DependencyGraph from "@/components/graph/DependencyGraph";

/**
 * Fetches blockers for all tasks in a project by calling task_blockers_list
 * for each task in parallel. Returns a map of taskId → BlockerItem[].
 */
async function fetchAllBlockers(
  taskIds: string[],
  projectPath: string,
): Promise<Map<string, BlockerItem[]>> {
  const result = new Map<string, BlockerItem[]>();
  if (taskIds.length === 0) return result;

  // Batch in groups of 20 to avoid overwhelming the server
  const BATCH_SIZE = 20;
  for (let i = 0; i < taskIds.length; i += BATCH_SIZE) {
    const batch = taskIds.slice(i, i + BATCH_SIZE);
    const responses = await Promise.all(
      batch.map(async (id) => {
        try {
          const response = await callMcpTool("task_blockers_list", {
            id,
            project: projectPath,
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
  const selectedProjectId = useSelectedProjectId();
  const projects = useProjects();

  const selectedProject = projects.find((p) => p.id === selectedProjectId);
  const projectPath = selectedProject?.path ?? null;

  // Collect IDs of non-epic tasks for blocker fetching
  const taskIds = useMemo(
    () => tasks.filter((t) => t.issue_type !== "epic").map((t) => t.id),
    [tasks],
  );

  // Fetch blockers for all tasks — cached and refreshed with tasks
  const { data: blockersByTask } = useQuery({
    queryKey: ["roadmap-blockers", projectPath, taskIds.join(",")],
    queryFn: () => fetchAllBlockers(taskIds, projectPath!),
    enabled: !!projectPath && taskIds.length > 0,
    staleTime: 30_000, // 30s — blockers change infrequently
    placeholderData: (prev) => prev,
  });

  const graphData = useMemo(
    () => toGraphData(tasks, epics, blockersByTask ?? new Map()),
    [tasks, epics, blockersByTask],
  );

  const hasData = graphData.some((g) => g.tasks.length > 0);

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
