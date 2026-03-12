import type { Task, TaskStatus } from '../test/fixtures'

type State = {
  tasks: Map<string, Task>
}

let state: State = { tasks: new Map() }

const api = {
  getState() {
    return {
      addTask(task: Task) {
        state.tasks.set(task.id, task)
      },
      getTask(id: string) {
        return state.tasks.get(id)
      },
      updateTask(id: string, update: Partial<Task>) {
        const existing = state.tasks.get(id)
        if (!existing) return
        state.tasks.set(id, { ...existing, ...update })
      },
      removeTask(id: string) {
        state.tasks.delete(id)
      },
      setTasks(tasks: Task[]) {
        state.tasks = new Map(tasks.map((t) => [t.id, t]))
      },
      clearTasks() {
        state.tasks.clear()
      },
      getAllTasks() {
        return [...state.tasks.values()]
      },
      getTasksByEpic(epicId: string) {
        return [...state.tasks.values()].filter((t) => t.epicId === epicId)
      },
      getTasksByStatus(status: TaskStatus) {
        return [...state.tasks.values()].filter((t) => t.status === status)
      },
    }
  },
}

export const taskStore = api
