/**
 * App types derived from MCP generated types.
 *
 * Overrides only `owner` (nullable in practice) and adds session
 * fields the API returns at runtime but aren't in the JSON schema.
 */

import type { TaskListOutputSchema, EpicListOutputSchema } from "./generated/mcp-tools.gen";

export type AcceptanceCriterion = TaskListOutputSchema.AcceptanceCriterionStatus;

export type Task = Omit<TaskListOutputSchema.TaskListItem, "owner"> & {
  owner: string | null;
  // Enriched by task_show and SSE events from agent sessions — not in task_list
  active_session?: { model_id?: string; started_at?: string } | null;
  session_count?: number;
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
