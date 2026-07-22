import "@testing-library/jest-dom/vitest"

// jsdom doesn't provide Web Streams API — polyfill so eventsource-parser (and anything
// else using TransformStream/ReadableStream/WritableStream) can load without crashing.
if (typeof globalThis.TransformStream === "undefined") {
  const streams = await import("node:stream/web");
  globalThis.TransformStream = streams.TransformStream as typeof globalThis.TransformStream;
  globalThis.ReadableStream = streams.ReadableStream as typeof globalThis.ReadableStream;
  globalThis.WritableStream = streams.WritableStream as typeof globalThis.WritableStream;
}

// jsdom also omits the Compression Streams API — the galaxy artifact client
// decodes an explicit gzip body via DecompressionStream. Expose the Node
// implementation so that code path is exercised under test.
if (typeof globalThis.DecompressionStream === "undefined") {
  const streams = await import("node:stream/web");
  globalThis.CompressionStream = streams.CompressionStream as typeof globalThis.CompressionStream;
  globalThis.DecompressionStream = streams.DecompressionStream as typeof globalThis.DecompressionStream;
}

// jsdom's Blob (v29) has no `.stream()`, which real browsers do and the galaxy
// artifact client uses to feed the gzip decoder. Swap in Node's spec-complete
// Blob so that decode path runs under test.
if (typeof (globalThis.Blob?.prototype as { stream?: unknown } | undefined)?.stream !== "function") {
  const { Blob } = await import("node:buffer");
  globalThis.Blob = Blob as unknown as typeof globalThis.Blob;
}

// jsdom doesn't provide WebGL2RenderingContext — sigma 3.x CJS build
// references it at import time; stub so the module can load.
if (typeof globalThis.WebGL2RenderingContext === "undefined") {
  // @ts-expect-error minimal stub for sigma import
  globalThis.WebGL2RenderingContext = class {};
}
if (typeof globalThis.WebGLRenderingContext === "undefined") {
  // @ts-expect-error minimal stub for sigma import
  globalThis.WebGLRenderingContext = class {};
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
type MockListener = (...args: unknown[]) => void;
const mockListeners = new Map<string, Set<MockListener>>();

Object.defineProperty(window, 'electronAPI', {
  value: {
    invoke: vi.fn().mockRejectedValue(new Error('invoke not mocked for this command')),
    on: vi.fn((event: string, callback: MockListener) => {
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
