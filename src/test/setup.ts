import "@testing-library/jest-dom/vitest"

// jsdom does not implement scrollIntoView; make it safe for components using autoscroll effects
if (!Element.prototype.scrollIntoView) {
  Object.defineProperty(Element.prototype, "scrollIntoView", {
    value: vi.fn(),
    writable: true,
    configurable: true,
  })
}

// Mock Tauri internals — prevents "window.__TAURI_INTERNALS__ is not defined" errors
Object.defineProperty(window, "__TAURI_INTERNALS__", {
  value: {
    invoke: () => Promise.resolve(),
    transformCallback: () => 0,
    metadata: { currentWebview: { label: "main" }, currentWindow: { label: "main" } },
  },
  writable: true,
})

// Mock Tauri core invoke — default no-op, tests override per-command
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn().mockRejectedValue(new Error("invoke not mocked for this command")),
}));

// Mock @tauri-apps/api
vi.mock("@tauri-apps/api", () => ({
  invoke: vi.fn(() => Promise.resolve()),
  transformCallback: vi.fn(() => 0),
}))

// Mock Tauri event system
const _listeners = new Map<string, Set<(event: unknown) => void>>();

export function emitTauriEvent(event: string, payload: unknown) {
  const handlers = _listeners.get(event);
  if (handlers) {
    handlers.forEach((fn) => fn({ payload }));
  }
}

export function clearTauriListeners() {
  _listeners.clear();
}

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((event: string, handler: (event: unknown) => void) => {
    if (!_listeners.has(event)) _listeners.set(event, new Set());
    _listeners.get(event)!.add(handler);
    const unlisten = () => {
      _listeners.get(event)?.delete(handler);
    };
    return Promise.resolve(unlisten);
  }),
}));

// Mock Tauri window API
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: vi.fn(() => ({
    show: vi.fn(),
    hide: vi.fn(),
    close: vi.fn(),
    setFocus: vi.fn(),
  })),
}));

// Mock @tauri-apps/plugin-opener
vi.mock("@tauri-apps/plugin-opener", () => ({
  open: vi.fn(() => Promise.resolve()),
}))

// Mock @tauri-apps/plugin-shell
vi.mock("@tauri-apps/plugin-shell", () => ({
  Command: class {
    static create() {
      return new this()
    }
    execute() {
      return Promise.resolve({ code: 0, stdout: "", stderr: "" })
    }
  },
  open: vi.fn(() => Promise.resolve()),
}))

// Mock SVG imports
vi.mock("@/assets/logo.svg", () => ({ default: "logo.svg" }));
