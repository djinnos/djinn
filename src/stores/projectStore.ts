import type { Project } from '../test/fixtures'

type State = {
  projects: Map<string, Project>
  selectedProjectId?: string
  projectViews: Map<string, string>
}

let state: State = {
  projects: new Map(),
  selectedProjectId: undefined,
  projectViews: new Map(),
}

export const projectStore = {
  getState() {
    return {
      setProjects(projects: Project[]) {
        state.projects = new Map(projects.map((p) => [p.id, p]))
      },
      addProject(project: Project) {
        state.projects.set(project.id, project)
      },
      removeProject(id: string) {
        state.projects.delete(id)
      },
      getProject(id: string) {
        return state.projects.get(id)
      },
      setSelectedProjectId(id?: string) {
        state.selectedProjectId = id
      },
      getSelectedProjectId() {
        return state.selectedProjectId
      },
      getSelectedProject() {
        if (!state.selectedProjectId) return undefined
        return state.projects.get(state.selectedProjectId)
      },
      setProjectView(projectId: string, view: string) {
        state.projectViews.set(projectId, view)
      },
      getProjectView(projectId: string) {
        return state.projectViews.get(projectId)
      },
      clearProjects() {
        state.projects.clear()
        state.selectedProjectId = undefined
        state.projectViews.clear()
      },
    }
  },
}
