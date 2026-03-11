/**
 * App types derived from MCP generated types.
 *
 * Overrides `owner` (nullable in practice) and adds `project_id`
 * which is stamped client-side.
 */

import type { TaskListOutputSchema, EpicListOutputSchema } from "./generated/mcp-tools.gen";

export type AcceptanceCriterion = TaskListOutputSchema.AcceptanceCriterionStatus;

export type Task = Omit<TaskListOutputSchema.TaskListItem, "owner"> & {
  owner: string | null;
  // Stamped by the desktop app when fetching from a specific project
  project_id?: string | null;
};

export type Epic = Omit<EpicListOutputSchema.EpicModel, "owner"> & {
  owner: string | null;
};

export interface Project {
  id: string;
  name: string;
  path?: string;
  description?: string;
}
