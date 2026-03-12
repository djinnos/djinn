import { describe, it, expect, beforeEach } from 'vitest'
import { epicStore } from './epicStore'
import { mockEpic, mockEpic2 } from '../test/fixtures'

describe('epicStore', () => {
  beforeEach(() => {
    epicStore.getState().clearEpics()
  })

  it('add/get epic', () => {
    const store = epicStore.getState()
    store.addEpic(mockEpic)
    expect(store.getEpic(mockEpic.id)).toEqual(mockEpic)
  })

  it('updateEpic merges fields', () => {
    const store = epicStore.getState()
    store.addEpic(mockEpic)
    store.updateEpic(mockEpic.id, { status: 'closed' })
    expect(store.getEpic(mockEpic.id)).toEqual({ ...mockEpic, status: 'closed' })
  })

  it('removeEpic removes from map', () => {
    const store = epicStore.getState()
    store.addEpic(mockEpic)
    store.removeEpic(mockEpic.id)
    expect(store.getEpic(mockEpic.id)).toBeUndefined()
  })

  it('setEpics replaces all', () => {
    const store = epicStore.getState()
    store.setEpics([mockEpic2])
    expect(store.getAllEpics()).toEqual([mockEpic2])
  })

  it('clearEpics empties', () => {
    const store = epicStore.getState()
    store.setEpics([mockEpic])
    store.clearEpics()
    expect(store.getAllEpics()).toEqual([])
  })

  it('getEpicsByStatus filters open/closed', () => {
    const store = epicStore.getState()
    store.setEpics([mockEpic, mockEpic2])
    expect(store.getEpicsByStatus('open')).toEqual([mockEpic])
    expect(store.getEpicsByStatus('closed')).toEqual([mockEpic2])
  })
})
