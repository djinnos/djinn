/**
 * SSE Event Handlers - Wire SSE events to task/epic stores
 *
 * Sets up subscriptions to SSE events and updates stores directly
 * from full-entity event payloads. No mapping needed — types match MCP wire format.
 */

import { sseStore, type SSEEvent } from "./sseStore";
import { taskStore } from "./taskStore";
import { epicStore } from "./epicStore";
import { projectStore } from "./projectStore";
import { queryClient } from "@/lib/queryClient";
import { fetchProjects } from "@/api/server";
import type { Task, Epic } from "@/api/types";

/**
 * Unwrap SSE event payload.
 * SSE events arrive as {type, action, data: {...entity...}}.
 * Returns the inner entity object.
 */
function unwrapPayload(raw: unknown): Record<string, unknown> {
  const obj = raw as Record<string, unknown>;
  if (obj && typeof obj === "object" && "data" in obj && typeof obj.data === "object") {
    return obj.data as Record<string, unknown>;
  }
  return obj;
}

/**
 * SSE sends some array fields as JSON strings (e.g. labels, acceptance_criteria).
 * Parse them back to arrays before storing.
 */
function normalizeSSEPayload(payload: Record<string, unknown>): Record<string, unknown> {
  const result = { ...payload };
  for (const key of ["labels", "acceptance_criteria", "memory_refs"]) {
    if (typeof result[key] === "string") {
      try {
        result[key] = JSON.parse(result[key] as string);
      } catch {
        // leave as-is
      }
    }
  }
  return result;
}

// Track subscription cleanup functions
let taskCreatedUnsub: (() => void) | null = null;
let taskUpdatedUnsub: (() => void) | null = null;
let taskDeletedUnsub: (() => void) | null = null;
let epicCreatedUnsub: (() => void) | null = null;
let epicUpdatedUnsub: (() => void) | null = null;
let epicDeletedUnsub: (() => void) | null = null;

/**
 * Initialize SSE event handlers
 * Call this once at app startup to wire SSE events to stores
 */
export function initSSEEventHandlers(): () => void {
  const { subscribe } = sseStore.getState();

  // Task events — SSE sends snake_case MCP payloads wrapped in {type,action,data}
  // Only apply events for the currently selected project to avoid cross-project flicker.
  taskCreatedUnsub = subscribe("task_created", (event: SSEEvent) => {
    const task = normalizeSSEPayload(unwrapPayload(event.data)) as unknown as Task;
    const selectedProject = projectStore.getState().getSelectedProject();
    if (selectedProject && task.project_id && task.project_id !== selectedProject.id) return;
    taskStore.getState().addTask(task);
    queryClient.setQueryData(["tasks"], (current: Task[] | undefined) =>
      current ? [...current, task] : [task]
    );
  });

  taskUpdatedUnsub = subscribe("task_updated", (event: SSEEvent) => {
    const task = normalizeSSEPayload(unwrapPayload(event.data)) as unknown as Task;
    const selectedProject = projectStore.getState().getSelectedProject();
    if (selectedProject && task.project_id && task.project_id !== selectedProject.id) return;

    // SSE task.updated payloads don't include active_session or session_count
    // (those are only added by MCP task_list/task_show). Preserve the values
    // that the session_started handler already set on the store.
    const existing = taskStore.getState().getTask(task.id);
    if (existing) {
      if (!("active_session" in task)) task.active_session = existing.active_session;
      if (!("session_count" in task)) task.session_count = existing.session_count;
      if (!("duration_seconds" in task)) task.duration_seconds = existing.duration_seconds;
    }

    taskStore.getState().updateTask(task);
    queryClient.setQueryData(["tasks"], (current: Task[] | undefined) =>
      current?.map((t) => (t.id === task.id ? task : t))
    );
  });

  taskDeletedUnsub = subscribe("task_deleted", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as { id: string };
    taskStore.getState().removeTask(payload.id);
    queryClient.setQueryData(["tasks"], (current: { id: string }[] | undefined) =>
      current?.filter((task) => task.id !== payload.id)
    );
  });

  // Epic events — SSE sends snake_case MCP payloads wrapped in {type,action,data}
  epicCreatedUnsub = subscribe("epic_created", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data);
    const selectedProject = projectStore.getState().getSelectedProject();
    if (selectedProject && payload.project_id && payload.project_id !== selectedProject.id) return;
    const epic = payload as unknown as Epic;
    epicStore.getState().addEpic(epic);
    queryClient.setQueryData(["epics"], (current: Epic[] | undefined) =>
      current ? [...current, epic] : [epic]
    );
  });

  epicUpdatedUnsub = subscribe("epic_updated", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data);
    const selectedProject = projectStore.getState().getSelectedProject();
    if (selectedProject && payload.project_id && payload.project_id !== selectedProject.id) return;
    const epic = payload as unknown as Epic;
    epicStore.getState().updateEpic(epic);
    queryClient.setQueryData(["epics"], (current: Epic[] | undefined) =>
      current?.map((e) => (e.id === epic.id ? epic : e))
    );
  });

  epicDeletedUnsub = subscribe("epic_deleted", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as { id: string };
    epicStore.getState().removeEpic(payload.id);
    queryClient.setQueryData(["epics"], (current: { id: string }[] | undefined) =>
      current?.filter((epic) => epic.id !== payload.id)
    );
  });

  // Session events — update active_session on the corresponding task
  const sessionDispatchedUnsub = subscribe("session_dispatched", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as {
      task_id?: string;
      agent_type?: string;
      model_id?: string;
    };
    if (!payload.task_id) return;
    const existing = taskStore.getState().getTask(payload.task_id);
    if (!existing) return;
    taskStore.getState().updateTask({
      ...existing,
      active_session: {
        session_id: undefined,
        agent_type: payload.agent_type,
        model_id: payload.model_id,
        started_at: new Date().toISOString(),
        status: "dispatched",
      },
    });
  });

  const sessionStartedUnsub = subscribe("session_started", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as {
      id?: string;
      task_id?: string;
      agent_type?: string;
      model_id?: string;
      started_at?: string;
      status?: string;
    };
    if (!payload.task_id) return;
    const existing = taskStore.getState().getTask(payload.task_id);
    if (!existing) return;
    taskStore.getState().updateTask({
      ...existing,
      active_session: {
        session_id: payload.id,
        agent_type: payload.agent_type,
        model_id: payload.model_id,
        started_at: payload.started_at,
        status: payload.status,
      },
    });
  });

  const sessionEndedUnsub = subscribe("session_ended", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as { task_id?: string };
    if (!payload.task_id) return;
    const existing = taskStore.getState().getTask(payload.task_id);
    if (!existing) return;
    taskStore.getState().updateTask({
      ...existing,
      active_session: undefined,
      session_count: (existing.session_count ?? 0) + 1,
    });
  });

  // Sync events — when an import brings in new tasks, the individual task.updated
  // SSE events (from_sync=true) will have already updated the stores. This handler
  // is for visibility — invalidate queries so any list views re-fetch.
  const syncCompletedUnsub = subscribe("sync_completed", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as {
      channel?: string;
      direction?: string;
      count?: number;
      error?: string | null;
    };
    // Only refresh on successful imports that actually changed data.
    if (payload.direction === "import" && (payload.count ?? 0) > 0) {
      queryClient.invalidateQueries({ queryKey: ["tasks"] });
      queryClient.invalidateQueries({ queryKey: ["epics"] });
    }
  });

  const invalidateSettingsLikeData = () => {
    queryClient.invalidateQueries({ queryKey: ["providers"] });
    queryClient.invalidateQueries({ queryKey: ["settings"] });
  };

  const projectChangedUnsub = subscribe("project_changed", () => {
    invalidateSettingsLikeData();
    // Refetch project list so the ProjectSelector updates when projects are added/removed
    fetchProjects()
      .then((projects) => projectStore.getState().setProjects(projects))
      .catch((err) => console.error("Failed to refetch projects after SSE event:", err));
  });

  // Return cleanup function
  return () => {
    taskCreatedUnsub?.();
    taskUpdatedUnsub?.();
    taskDeletedUnsub?.();
    epicCreatedUnsub?.();
    epicUpdatedUnsub?.();
    epicDeletedUnsub?.();
    projectChangedUnsub?.();
    sessionDispatchedUnsub?.();
    sessionStartedUnsub?.();
    sessionEndedUnsub?.();
    syncCompletedUnsub?.();
  };
}

/**
 * Cleanup SSE event handlers
 */
export function cleanupSSEEventHandlers(): void {
  taskCreatedUnsub?.();
  taskUpdatedUnsub?.();
  taskDeletedUnsub?.();
  epicCreatedUnsub?.();
  epicUpdatedUnsub?.();
  epicDeletedUnsub?.();

  taskCreatedUnsub = null;
  taskUpdatedUnsub = null;
  taskDeletedUnsub = null;
  epicCreatedUnsub = null;
  epicUpdatedUnsub = null;
  epicDeletedUnsub = null;
}
