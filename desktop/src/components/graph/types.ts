import type { Task } from "@/api/types";

/**
 * A task within an epic, shaped for the graph view.
 */
export interface TaskData {
  id: string;
  title: string;
  status: string;
  description?: string;
  /** IDs of tasks this task depends on (blockers) */
  dependencies?: string[];
  /** Full task object for rendering details */
  task: Task;
}

/**
 * An epic containing tasks, shaped for the graph view.
 * Each epic becomes a group node in the ReactFlow graph.
 */
export interface EpicData {
  id: string;
  name: string;
  /** Hex color string from the epic (e.g., "#8b5cf6") */
  color: string;
  /** Unicode emoji from the epic (e.g., "🔐") */
  emoji: string;
  /** Epic status — used to visually distinguish closed epics */
  status?: string;
  tasks: TaskData[];
}
