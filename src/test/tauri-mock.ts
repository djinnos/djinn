import { vi } from "vitest"

type InvokeHandler = (cmd: string, args?: Record<string, unknown>) => unknown
type EventCallback = (event: { payload: unknown }) => void

let invokeHandler: InvokeHandler = () => undefined

const listeners = new Map<string, Set<EventCallback>>()

export const mockInvoke = vi.fn(
  (cmd: string, args?: Record<string, unknown>) => {
    return Promise.resolve(invokeHandler(cmd, args))
  },
)

export const mockListen = vi.fn(
  (event: string, handler: EventCallback) => {
    if (!listeners.has(event)) {
      listeners.set(event, new Set())
    }
    listeners.get(event)!.add(handler)
    const unlisten = () => {
      listeners.get(event)?.delete(handler)
    }
    return Promise.resolve(unlisten)
  },
)

export const mockEmit = vi.fn(
  (event: string, payload?: unknown) => {
    const handlers = listeners.get(event)
    if (handlers) {
      for (const handler of handlers) {
        handler({ payload })
      }
    }
    return Promise.resolve()
  },
)

export function setInvokeHandler(handler: InvokeHandler) {
  invokeHandler = handler
}

export function resetTauriMocks() {
  mockInvoke.mockClear()
  mockListen.mockClear()
  mockEmit.mockClear()
  listeners.clear()
  invokeHandler = () => undefined
}

vi.mock("@tauri-apps/api/core", () => ({
  invoke: mockInvoke,
}))

vi.mock("@tauri-apps/api/event", () => ({
  listen: mockListen,
  emit: mockEmit,
}))
