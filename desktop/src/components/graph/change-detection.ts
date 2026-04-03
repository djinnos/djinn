import type { EpicData, TaskData } from "./types";

export interface ChangeDetectionResult {
  isStructuralChange: boolean;
  changedTaskIds: string[];
}

/**
 * Builds a stable string fingerprint from task identity fields.
 * When two EpicData[] arrays produce the same fingerprint, no diffing needed.
 */
export function fingerprintEpics(epics: EpicData[]): string {
  const parts: string[] = [];
  for (const epic of epics) {
    parts.push(`E:${epic.id}`);
    for (const task of epic.tasks) {
      parts.push(
        `T:${task.id}:${task.status}:${task.title}:${task.description ?? ""}`,
      );
      if (task.dependencies && task.dependencies.length > 0) {
        parts.push(`D:${[...task.dependencies].sort().join(",")}`);
      }
    }
  }
  return parts.join("|");
}

/**
 * Detects whether changes between two EpicData[] states are structural or data-only.
 *
 * Structural changes require full ELK re-layout (task/epic added/removed, deps changed).
 * Data-only changes update node data without re-layout (status/title changes).
 */
export function detectStructuralChanges(
  prev: EpicData[],
  next: EpicData[],
): ChangeDetectionResult {
  const prevFp = fingerprintEpics(prev);
  const nextFp = fingerprintEpics(next);
  if (prevFp === nextFp) {
    return { isStructuralChange: false, changedTaskIds: [] };
  }

  const prevEpicIds = new Set(prev.map((e) => e.id));
  const nextEpicIds = new Set(next.map((e) => e.id));

  if (prevEpicIds.size !== nextEpicIds.size) {
    return { isStructuralChange: true, changedTaskIds: [] };
  }

  for (const id of prevEpicIds) {
    if (!nextEpicIds.has(id)) {
      return { isStructuralChange: true, changedTaskIds: [] };
    }
  }

  const prevTaskMap = buildTaskMap(prev);
  const nextTaskMap = buildTaskMap(next);

  const prevTaskIds = new Set(prevTaskMap.keys());
  const nextTaskIds = new Set(nextTaskMap.keys());

  if (prevTaskIds.size !== nextTaskIds.size) {
    return { isStructuralChange: true, changedTaskIds: [] };
  }

  for (const id of prevTaskIds) {
    if (!nextTaskIds.has(id)) {
      return { isStructuralChange: true, changedTaskIds: [] };
    }
  }

  const changedTaskIds: string[] = [];

  for (const [taskId, prevTask] of prevTaskMap) {
    const nextTask = nextTaskMap.get(taskId)!;

    if (prevTask.epicId !== nextTask.epicId) {
      return { isStructuralChange: true, changedTaskIds: [] };
    }

    const prevDeps = new Set(prevTask.dependencies);
    const nextDeps = new Set(nextTask.dependencies);

    if (prevDeps.size !== nextDeps.size) {
      return { isStructuralChange: true, changedTaskIds: [] };
    }

    for (const dep of prevDeps) {
      if (!nextDeps.has(dep)) {
        return { isStructuralChange: true, changedTaskIds: [] };
      }
    }

    if (
      prevTask.title !== nextTask.title ||
      prevTask.status !== nextTask.status ||
      prevTask.description !== nextTask.description
    ) {
      changedTaskIds.push(taskId);
    }
  }

  return { isStructuralChange: false, changedTaskIds };
}

interface TaskWithEpic extends TaskData {
  epicId: string;
}

function buildTaskMap(epics: EpicData[]): Map<string, TaskWithEpic> {
  const map = new Map<string, TaskWithEpic>();
  for (const epic of epics) {
    for (const task of epic.tasks) {
      map.set(task.id, { ...task, epicId: epic.id });
    }
  }
  return map;
}
