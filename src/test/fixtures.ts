export type TaskStatus = 'todo' | 'in_progress' | 'done'
export type EpicStatus = 'open' | 'closed'

export interface Task {
  id: string
  title: string
  epicId: string
  status: TaskStatus
}

export interface Epic {
  id: string
  title: string
  status: EpicStatus
}

export interface Project {
  id: string
  name: string
}

export const mockTask: Task = {
  id: 'task-1',
  title: 'Mock task',
  epicId: 'epic-1',
  status: 'todo',
}

export const mockTask2: Task = {
  id: 'task-2',
  title: 'Mock task 2',
  epicId: 'epic-2',
  status: 'done',
}

export const mockEpic: Epic = {
  id: 'epic-1',
  title: 'Mock epic',
  status: 'open',
}

export const mockEpic2: Epic = {
  id: 'epic-2',
  title: 'Mock epic 2',
  status: 'closed',
}

export const mockProject: Project = {
  id: 'project-1',
  name: 'Mock project',
}

export const mockProject2: Project = {
  id: 'project-2',
  name: 'Mock project 2',
}
