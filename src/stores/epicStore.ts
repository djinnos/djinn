import type { Epic, EpicStatus } from '../test/fixtures'

type State = { epics: Map<string, Epic> }
let state: State = { epics: new Map() }

export const epicStore = {
  getState() {
    return {
      addEpic(epic: Epic) {
        state.epics.set(epic.id, epic)
      },
      getEpic(id: string) {
        return state.epics.get(id)
      },
      updateEpic(id: string, update: Partial<Epic>) {
        const existing = state.epics.get(id)
        if (!existing) return
        state.epics.set(id, { ...existing, ...update })
      },
      removeEpic(id: string) {
        state.epics.delete(id)
      },
      setEpics(epics: Epic[]) {
        state.epics = new Map(epics.map((e) => [e.id, e]))
      },
      clearEpics() {
        state.epics.clear()
      },
      getAllEpics() {
        return [...state.epics.values()]
      },
      getEpicsByStatus(status: EpicStatus) {
        return [...state.epics.values()].filter((e) => e.status === status)
      },
    }
  },
}
