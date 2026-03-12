import { describe, it, expect, beforeEach } from 'vitest'
import { taskStore } from './taskStore'
import { mockTask, mockTask2 } from '../test/fixtures'

describe('taskStore', () => {
  beforeEach(() => {
    taskStore.getState().clearTasks()
  })

  it('addTask adds to map, retrievable by ID', () => {
    const store = taskStore.getState()
    store.addTask(mockTask)
    expect(store.getTask(mockTask.id)).toEqual(mockTask)
  })

  it('updateTask merges fields (partial update)', () => {
    const store = taskStore.getState()
    store.addTask(mockTask)
    store.updateTask(mockTask.id, { status: 'in_progress' })
    expect(store.getTask(mockTask.id)).toEqual({ ...mockTask, status: 'in_progress' })
  })

  it('removeTask removes from map', () => {
    const store = taskStore.getState()
    store.addTask(mockTask)
    store.removeTask(mockTask.id)
    expect(store.getTask(mockTask.id)).toBeUndefined()
  })

  it('setTasks bulk replaces all tasks', () => {
    const store = taskStore.getState()
    store.addTask(mockTask)
    store.setTasks([mockTask2])
    expect(store.getTask(mockTask.id)).toBeUndefined()
    expect(store.getTask(mockTask2.id)).toEqual(mockTask2)
  })

  it('getTasksByEpic filters correctly', () => {
    const store = taskStore.getState()
    store.setTasks([mockTask, mockTask2])
    expect(store.getTasksByEpic('epic-1')).toEqual([mockTask])
  })

  it('getTasksByStatus filters correctly', () => {
    const store = taskStore.getState()
    store.setTasks([mockTask, mockTask2])
    expect(store.getTasksByStatus('done')).toEqual([mockTask2])
  })

  it('clearTasks empties the map', () => {
    const store = taskStore.getState()
    store.setTasks([mockTask])
    store.clearTasks()
    expect(store.getAllTasks()).toEqual([])
  })

  it('getAllTasks returns array of all tasks', () => {
    const store = taskStore.getState()
    store.setTasks([mockTask, mockTask2])
    expect(store.getAllTasks()).toEqual([mockTask, mockTask2])
  })
})
