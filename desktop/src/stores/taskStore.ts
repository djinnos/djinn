/**
 * Task Store - Vanilla createStore for task state management
 *
 * Uses subscribeWithSelector for granular subscriptions.
 * Updated directly from SSE event payloads (full-entity events).
 */

import { createStore } from "zustand/vanilla";
import { subscribeWithSelector } from "zustand/middleware";
import type { Task } from "@/api/types";

export interface TaskState {
  tasks: Map<string, Task>;

  addTask: (task: Task) => void;
  updateTask: (task: Task) => void;
  removeTask: (id: string) => void;

  getTask: (id: string) => Task | undefined;
  getTasksByEpic: (epicId: string) => Task[];
  getTasksByStatus: (status: string) => Task[];
  getAllTasks: () => Task[];
  clearTasks: () => void;

  setTasks: (tasks: Task[]) => void;
}

export const taskStore = createStore<TaskState>()(
  subscribeWithSelector((set, get) => ({
    tasks: new Map(),

    addTask: (task) => {
      set((state) => {
        const newTasks = new Map(state.tasks);
        newTasks.set(task.id, task);
        return { tasks: newTasks };
      });
    },

    updateTask: (task) => {
      set((state) => {
        const newTasks = new Map(state.tasks);
        newTasks.set(task.id, task);
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
        (task) => task.epic_id === epicId
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
