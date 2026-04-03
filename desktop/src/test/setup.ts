import "@testing-library/jest-dom/vitest"

// jsdom doesn't provide Web Streams API — polyfill so eventsource-parser (and anything
// else using TransformStream/ReadableStream/WritableStream) can load without crashing.
if (typeof globalThis.TransformStream === "undefined") {
  const streams = await import("node:stream/web");
  globalThis.TransformStream = streams.TransformStream as typeof globalThis.TransformStream;
  globalThis.ReadableStream = streams.ReadableStream as typeof globalThis.ReadableStream;
  globalThis.WritableStream = streams.WritableStream as typeof globalThis.WritableStream;
}

// jsdom does not implement scrollIntoView; make it safe for components using autoscroll effects
if (!Element.prototype.scrollIntoView) {
  Object.defineProperty(Element.prototype, "scrollIntoView", {
    value: vi.fn(),
    writable: true,
    configurable: true,
  })
}

// Mock Electron API — the shim layer (`@/electron/shims/*`) delegates to
// `window.electronAPI.*` at runtime, so mocking at this boundary is sufficient.
const mockListeners = new Map<string, Set<Function>>();

Object.defineProperty(window, 'electronAPI', {
  value: {
    invoke: vi.fn().mockRejectedValue(new Error('invoke not mocked for this command')),
    on: vi.fn((event: string, callback: Function) => {
      if (!mockListeners.has(event)) mockListeners.set(event, new Set());
      mockListeners.get(event)!.add(callback);
      return Promise.resolve(() => { mockListeners.get(event)?.delete(callback); });
    }),
    getWindow: vi.fn(() => ({
      minimize: vi.fn(),
      toggleMaximize: vi.fn(),
      close: vi.fn(),
      startDragging: vi.fn(),
    })),
  },
  writable: true,
});

// Test utility: emit events to registered listeners
export function emitMockEvent(event: string, payload: unknown) {
  mockListeners.get(event)?.forEach(cb => cb(payload));
}

export function clearMockListeners() {
  mockListeners.clear();
}


// Mock SVG imports
vi.mock("@/assets/logo.svg", () => ({ default: "logo.svg" }));
