import { vi } from "vitest"

type InvokeHandler = (cmd: string, args?: Record<string, unknown>) => unknown
type EventCallback = (event: { payload: unknown }) => void

let invokeHandler: InvokeHandler = () => undefined

const listeners = new Map<string, Set<EventCallback>>()

/**
 * Mock for `window.electronAPI.invoke` — resolves via the registered handler.
 */
export const mockInvoke = vi.fn(
  (cmd: string, args?: Record<string, unknown>) => {
    return Promise.resolve(invokeHandler(cmd, args))
  },
)

/**
 * Mock for `window.electronAPI.on` — registers event listeners.
 */
export const mockOn = vi.fn(
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

/**
 * Emit a mock event to all registered listeners for the given event name.
 */
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

/**
 * Set a custom handler for `invoke` calls.
 */
export function setInvokeHandler(handler: InvokeHandler) {
  invokeHandler = handler
}

/**
 * Reset all electron mocks and listeners.
 */
export function resetElectronMocks() {
  mockInvoke.mockClear()
  mockOn.mockClear()
  mockEmit.mockClear()
  listeners.clear()
  invokeHandler = () => undefined
}

/**
 * Install the mocks onto `window.electronAPI`.
 * Call this in a `beforeEach` or at module scope to override the global setup.ts mock.
 */
export function installElectronMocks() {
  Object.defineProperty(window, 'electronAPI', {
    value: {
      invoke: mockInvoke,
      on: mockOn,
      getWindow: vi.fn(() => ({
        minimize: vi.fn(),
        toggleMaximize: vi.fn(),
        close: vi.fn(),
        startDragging: vi.fn(),
      })),
    },
    writable: true,
  })
}
