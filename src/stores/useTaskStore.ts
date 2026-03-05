/**
 * useTaskStore - React hook for task state with selector support
 * 
 * Uses subscribeWithSelector for granular, performant subscriptions.
 * Components re-render only when their selected slice changes.
 */

import { useCallback } from "react";
import { useStoreWithSelector } from "./useStoreWithSelector";
import { taskStore, type TaskState } from "./taskStore";
import type { Task } from "../types";

// Re-export the store for direct access
export { taskStore } from "./taskStore";

// Helper hook for subscribing with selectors
export function useTaskStore(): TaskState;
export function useTaskStore<T>(selector: (state: TaskState) => T): T;
export function useTaskStore<T>(selector?: (state: TaskState) => T): TaskState | T {
  return useStoreWithSelector(taskStore, selector);
}

// Convenience hooks for common selections
export function useTask(id: string): Task | undefined {
  return useTaskStore(
    useCallback((state) => state.tasks.get(id), [id])
  );
}

export function useTasksByEpic(epicId: string): Task[] {
  return useTaskStore(
    useCallback((state) => state.getTasksByEpic(epicId), [epicId])
  );
}

export function useTasksByStatus(status: Task['status']): Task[] {
  return useTaskStore(
    useCallback((state) => state.getTasksByStatus(status), [status])
  );
}

export function useAllTasks(): Task[] {
  return useTaskStore((state) => state.getAllTasks());
}

export function useTaskCount(): number {
  return useTaskStore((state) => state.tasks.size);
}
