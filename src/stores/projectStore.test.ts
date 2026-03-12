import { describe, it, expect, beforeEach } from 'vitest'
import { projectStore } from './projectStore'
import { mockProject, mockProject2 } from '../test/fixtures'

describe('projectStore', () => {
  beforeEach(() => {
    projectStore.getState().clearProjects()
  })

  it('setProjects / addProject / removeProject', () => {
    const store = projectStore.getState()
    store.setProjects([mockProject])
    store.addProject(mockProject2)
    expect(store.getProject(mockProject.id)).toEqual(mockProject)
    expect(store.getProject(mockProject2.id)).toEqual(mockProject2)
    store.removeProject(mockProject.id)
    expect(store.getProject(mockProject.id)).toBeUndefined()
  })

  it('setSelectedProjectId updates selection', () => {
    const store = projectStore.getState()
    store.setSelectedProjectId(mockProject.id)
    expect(store.getSelectedProjectId()).toBe(mockProject.id)
  })

  it('getSelectedProject derives from projects + selectedId', () => {
    const store = projectStore.getState()
    store.setProjects([mockProject])
    store.setSelectedProjectId(mockProject.id)
    expect(store.getSelectedProject()).toEqual(mockProject)
  })

  it('projectViews per-project last view tracking', () => {
    const store = projectStore.getState()
    store.setProjectView(mockProject.id, 'board')
    store.setProjectView(mockProject2.id, 'list')
    expect(store.getProjectView(mockProject.id)).toBe('board')
    expect(store.getProjectView(mockProject2.id)).toBe('list')
  })
})
