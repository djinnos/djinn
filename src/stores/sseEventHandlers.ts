/**
 * SSE Event Handlers - Wire SSE events to task/epic stores
 * 
 * Sets up subscriptions to SSE events and updates stores directly
 * from full-entity event payloads. No follow-up reads needed.
 */

import { sseStore, type SSEEvent } from "./sseStore";
import { taskStore } from "./taskStore";
import { epicStore } from "./epicStore";
import { queryClient } from "@/lib/queryClient";
import { mapTaskFromMcp, mapEpicFromMcp } from "@/api/server";
import type {
  Task,
  Epic,
  TaskDeletedPayload,
  EpicDeletedPayload
} from "../types";

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
 * Parse them back to arrays before passing to the mapper.
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
  taskCreatedUnsub = subscribe("task_created", (event: SSEEvent) => {
    const raw = normalizeSSEPayload(unwrapPayload(event.data));
    const task: Task = mapTaskFromMcp(raw as any);
    taskStore.getState().addTask(task);
    queryClient.setQueryData(["tasks"], (current: Task[] | undefined) =>
      current ? [...current, task] : [task]
    );
  });

  taskUpdatedUnsub = subscribe("task_updated", (event: SSEEvent) => {
    const raw = normalizeSSEPayload(unwrapPayload(event.data));
    const task: Task = mapTaskFromMcp(raw as any);
    taskStore.getState().updateTask(task);
    queryClient.setQueryData(["tasks"], (current: Task[] | undefined) =>
      current?.map((t) => (t.id === task.id ? task : t))
    );
  });

  taskDeletedUnsub = subscribe("task_deleted", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as unknown as TaskDeletedPayload;
    taskStore.getState().removeTask(payload.id);
    queryClient.setQueryData(["tasks"], (current: { id: string }[] | undefined) =>
      current?.filter((task) => task.id !== payload.id)
    );
  });

  // Epic events — SSE sends snake_case MCP payloads wrapped in {type,action,data}
  epicCreatedUnsub = subscribe("epic_created", (event: SSEEvent) => {
    const raw = unwrapPayload(event.data);
    const epic: Epic = mapEpicFromMcp(raw as any);
    epicStore.getState().addEpic(epic);
    queryClient.setQueryData(["epics"], (current: Epic[] | undefined) =>
      current ? [...current, epic] : [epic]
    );
  });

  epicUpdatedUnsub = subscribe("epic_updated", (event: SSEEvent) => {
    const raw = unwrapPayload(event.data);
    const epic: Epic = mapEpicFromMcp(raw as any);
    epicStore.getState().updateEpic(epic);
    queryClient.setQueryData(["epics"], (current: Epic[] | undefined) =>
      current?.map((e) => (e.id === epic.id ? epic : e))
    );
  });

  epicDeletedUnsub = subscribe("epic_deleted", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as unknown as EpicDeletedPayload;
    epicStore.getState().removeEpic(payload.id);
    queryClient.setQueryData(["epics"], (current: { id: string }[] | undefined) =>
      current?.filter((epic) => epic.id !== payload.id)
    );
  });

  const invalidateSettingsLikeData = () => {
    queryClient.invalidateQueries({ queryKey: ["providers"] });
    queryClient.invalidateQueries({ queryKey: ["settings"] });
  };

  const projectChangedUnsub = subscribe("project_changed", () => {
    invalidateSettingsLikeData();
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
