import "@testing-library/jest-dom/vitest"

// Mock Tauri internals — prevents "window.__TAURI_INTERNALS__ is not defined" errors
Object.defineProperty(window, "__TAURI_INTERNALS__", {
  value: {
    invoke: () => Promise.resolve(),
    transformCallback: () => 0,
    metadata: { currentWebview: { label: "main" }, currentWindow: { label: "main" } },
  },
  writable: true,
})

// Mock @tauri-apps/api
vi.mock("@tauri-apps/api", () => ({
  invoke: vi.fn(() => Promise.resolve()),
  transformCallback: vi.fn(() => 0),
}))

// Mock @tauri-apps/api/event
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
  emit: vi.fn(() => Promise.resolve()),
  once: vi.fn(() => Promise.resolve(() => {})),
}))

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
