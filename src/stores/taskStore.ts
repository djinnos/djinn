/**
 * Task Store - Vanilla createStore for task state management
 * 
 * Uses subscribeWithSelector for granular subscriptions.
 * Updated directly from SSE event payloads (full-entity events).
 */

import { createStore } from "zustand/vanilla";
import { subscribeWithSelector } from "zustand/middleware";
import type { Task, TaskCreatedPayload, TaskUpdatedPayload } from "../types";

export interface TaskState {
  // Tasks map for O(1) lookups
  tasks: Map<string, Task>;
  
  // CRUD operations
  addTask: (payload: TaskCreatedPayload) => void;
  updateTask: (payload: TaskUpdatedPayload) => void;
  removeTask: (id: string) => void;
  
  // Utility operations
  getTask: (id: string) => Task | undefined;
  getTasksByEpic: (epicId: string) => Task[];
  getTasksByStatus: (status: Task['status']) => Task[];
  getAllTasks: () => Task[];
  clearTasks: () => void;
  
  // Batch operations
  setTasks: (tasks: Task[]) => void;
}

export const taskStore = createStore<TaskState>()(
  subscribeWithSelector((set, get) => ({
    tasks: new Map(),

    addTask: (payload) => {
      set((state) => {
        const newTasks = new Map(state.tasks);
        newTasks.set(payload.id, payload);
        return { tasks: newTasks };
      });
    },

    updateTask: (payload) => {
      set((state) => {
        const newTasks = new Map(state.tasks);
        newTasks.set(payload.id, payload);
        return { tasks: newTasks };
      });
    },

    removeTask: (id) => {
      set((state) => {
        const newTasks = new Map(state.tasks);
        newTasks.delete(id);
        return { tasks: newTasks };
      });
    },

    getTask: (id) => {
      return get().tasks.get(id);
    },

    getTasksByEpic: (epicId) => {
      return Array.from(get().tasks.values()).filter(
        (task) => task.epicId === epicId
      );
    },

    getTasksByStatus: (status) => {
      return Array.from(get().tasks.values()).filter(
        (task) => task.status === status
      );
    },

    getAllTasks: () => {
      return Array.from(get().tasks.values());
    },

    clearTasks: () => {
      set({ tasks: new Map() });
    },

    setTasks: (tasks) => {
      set({
        tasks: new Map(tasks.map((task) => [task.id, task])),
      });
    },
  }))
);

// React hook for components (with selector support)
export { useTaskStore } from "./useTaskStore";
