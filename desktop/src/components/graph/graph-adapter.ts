import type { Epic, Task } from "@/api/types";
import type { EpicData, TaskData } from "./types";

/** Default color when epic has no color set */
const DEFAULT_EPIC_COLOR = "#6b7280"; // neutral grey

/** Default emoji when epic has no emoji set */
const DEFAULT_EPIC_EMOJI = "📦";

/**
 * Blocker shape returned by the server's task_blockers_list endpoint.
 */
export interface BlockerItem {
  blocking_task_id: string;
  blocking_task_short_id: string;
  blocking_task_title: string;
  blocking_task_status: string;
  resolved: boolean;
}

function toTaskData(
  task: Task,
  blockersByTask: Map<string, BlockerItem[]>,
): TaskData {
  const blockers = blockersByTask.get(task.id) ?? [];
  return {
    id: task.id,
    title: task.title,
    status: task.status,
    description: task.description || "",
    task,
    dependencies: [
      ...new Set(blockers.map((b) => b.blocking_task_id).filter(Boolean)),
    ],
  };
}

/**
 * Converts tasks + epics into the graph's EpicData[] format.
 *
 * - Groups tasks under their parent epic
 * - Extracts blocker IDs as dependencies for edge drawing
 * - Tasks without an epic go into an "Unassigned" group
 */
export function toGraphData(
  tasks: Task[],
  epics: Map<string, Epic>,
  blockersByTask: Map<string, BlockerItem[]>,
): EpicData[] {
  const tasksByEpic = new Map<string, Task[]>();

  for (const task of tasks) {
    // Skip epic-type tasks themselves
    if (task.issue_type === "epic") continue;
    const epicKey = task.epic_id ?? "no-epic";
    const group = tasksByEpic.get(epicKey) || [];
    group.push(task);
    tasksByEpic.set(epicKey, group);
  }

  const result: EpicData[] = [];

  // Add epic groups
  for (const [epicId, epic] of epics) {
    if (epic.status === "closed") continue;
    const epicTasks = tasksByEpic.get(epicId) || [];
    // Skip closed epics with no tasks
    if (epicTasks.length === 0 && epic.status !== "open" && epic.status !== "drafting") continue;

    result.push({
      id: epicId,
      name: epic.title,
      color: epic.color || DEFAULT_EPIC_COLOR,
      emoji: epic.emoji || DEFAULT_EPIC_EMOJI,
      tasks: epicTasks.map((t) => toTaskData(t, blockersByTask)),
    });
  }

  // Add unassigned tasks group
  const unassignedTasks = tasksByEpic.get("no-epic") || [];
  // Filter out closed/done tasks from unassigned
  const visibleUnassigned = unassignedTasks.filter(
    (t) => t.status !== "closed",
  );
  if (visibleUnassigned.length > 0) {
    result.push({
      id: "no-epic",
      name: "Unassigned",
      color: "#6b7280",
      emoji: "📥",
      tasks: visibleUnassigned.map((t) => toTaskData(t, blockersByTask)),
    });
  }

  return result;
}
