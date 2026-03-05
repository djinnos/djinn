import { useMemo } from "react";
import { useAllEpics } from "@/stores/useEpicStore";
import { useAllTasks } from "@/stores/useTaskStore";
import type { Epic, Task } from "@/types";

export interface RoadmapEpic extends Epic {
  tasks: Task[];
}

export function useRoadmapData(): RoadmapEpic[] {
  const epics = useAllEpics();
  const tasks = useAllTasks();

  return useMemo(() => {
    const tasksByEpic = new Map<string, Task[]>();

    for (const task of tasks) {
      if (!task.epicId) continue;
      const list = tasksByEpic.get(task.epicId);
      if (list) {
        list.push(task);
      } else {
        tasksByEpic.set(task.epicId, [task]);
      }
    }

    return epics.map((epic) => ({
      ...epic,
      tasks: tasksByEpic.get(epic.id) ?? [],
    }));
  }, [epics, tasks]);
}
