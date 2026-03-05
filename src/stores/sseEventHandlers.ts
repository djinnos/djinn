/**
 * SSE Event Handlers - Wire SSE events to task/epic stores
 * 
 * Sets up subscriptions to SSE events and updates stores directly
 * from full-entity event payloads. No follow-up reads needed.
 */

import { sseStore, type SSEEvent } from "./sseStore";
import { taskStore } from "./taskStore";
import { epicStore } from "./epicStore";
import type { 
  TaskCreatedPayload, 
  TaskUpdatedPayload, 
  TaskDeletedPayload,
  EpicCreatedPayload,
  EpicUpdatedPayload,
  EpicDeletedPayload 
} from "../types";

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

  // Task events
  taskCreatedUnsub = subscribe("task_created", (event: SSEEvent) => {
    const payload = event.data as TaskCreatedPayload;
    taskStore.getState().addTask(payload);
  });

  taskUpdatedUnsub = subscribe("task_updated", (event: SSEEvent) => {
    const payload = event.data as TaskUpdatedPayload;
    taskStore.getState().updateTask(payload);
  });

  taskDeletedUnsub = subscribe("task_deleted", (event: SSEEvent) => {
    const payload = event.data as TaskDeletedPayload;
    taskStore.getState().removeTask(payload.id);
  });

  // Epic events
  epicCreatedUnsub = subscribe("epic_created", (event: SSEEvent) => {
    const payload = event.data as EpicCreatedPayload;
    epicStore.getState().addEpic(payload);
  });

  epicUpdatedUnsub = subscribe("epic_updated", (event: SSEEvent) => {
    const payload = event.data as EpicUpdatedPayload;
    epicStore.getState().updateEpic(payload);
  });

  epicDeletedUnsub = subscribe("epic_deleted", (event: SSEEvent) => {
    const payload = event.data as EpicDeletedPayload;
    epicStore.getState().removeEpic(payload.id);
  });

  // Return cleanup function
  return () => {
    taskCreatedUnsub?.();
    taskUpdatedUnsub?.();
    taskDeletedUnsub?.();
    epicCreatedUnsub?.();
    epicUpdatedUnsub?.();
    epicDeletedUnsub?.();
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
