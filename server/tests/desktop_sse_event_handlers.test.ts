/**
 * Desktop-side Vitest tests for SSE Event Handlers - SYNC-TEST-03
 *
 * **NOTE:** This file is meant for the desktop project at:
 *   desktop/src/stores/sseEventHandlers.test.ts
 *
 * It is placed here temporarily because the agent workspace is restricted to the
 * server repo. Please move this file to the desktop project's src/stores/ directory
 * and run with: `pnpm vitest run` (after adding vitest to devDependencies).
 *
 * AC #7: Test desktop sync_completed handler invalidates queries only on import with count > 0
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// Mock queryClient
const mockInvalidateQueries = vi.fn();
const mockSetQueryData = vi.fn();

vi.mock("@/lib/queryClient", () => ({
  queryClient: {
    invalidateQueries: mockInvalidateQueries,
    setQueryData: mockSetQueryData,
  },
}));

// Mock stores
const mockAddTask = vi.fn();
const mockUpdateTask = vi.fn();
const mockRemoveTask = vi.fn();
const mockAddEpic = vi.fn();
const mockUpdateEpic = vi.fn();
const mockRemoveEpic = vi.fn();
const mockGetSelectedProject = vi.fn(() => null);
const mockSetProjects = vi.fn();

vi.mock("../stores/taskStore", () => ({
  taskStore: {
    getState: () => ({
      addTask: mockAddTask,
      updateTask: mockUpdateTask,
      removeTask: mockRemoveTask,
    }),
  },
}));

vi.mock("../stores/epicStore", () => ({
  epicStore: {
    getState: () => ({
      addEpic: mockAddEpic,
      updateEpic: mockUpdateEpic,
      removeEpic: mockRemoveEpic,
    }),
  },
}));

vi.mock("../stores/projectStore", () => ({
  projectStore: {
    getState: () => ({
      getSelectedProject: mockGetSelectedProject,
      setProjects: mockSetProjects,
    }),
  },
}));

vi.mock("@/api/server", () => ({
  fetchProjects: () => Promise.resolve([]),
}));

// SSE Store mock with event emitter pattern
type EventHandler = (event: { type: string; data: unknown }) => void;
const eventHandlers: Map<string, EventHandler[]> = new Map();

vi.mock("../stores/sseStore", () => ({
  sseStore: {
    getState: () => ({
      subscribe: (event: string, handler: EventHandler) => {
        if (!eventHandlers.has(event)) {
          eventHandlers.set(event, []);
        }
        eventHandlers.get(event)!.push(handler);
        return () => {
          const handlers = eventHandlers.get(event);
          if (handlers) {
            const idx = handlers.indexOf(handler);
            if (idx > -1) handlers.splice(idx, 1);
          }
        };
      },
      emit: (event: string, data: unknown) => {
        const handlers = eventHandlers.get(event) || [];
        handlers.forEach((h) => h({ type: event, data }));
      },
    }),
  },
}));

// Import after mocks are set up
import { initSSEEventHandlers, cleanupSSEEventHandlers } from "./sseEventHandlers";

describe("SSE Event Handlers - sync_completed (AC #7)", () => {
  let cleanup: (() => void) | null = null;

  beforeEach(() => {
    vi.clearAllMocks();
    eventHandlers.clear();
    cleanup = initSSEEventHandlers();
  });

  afterEach(() => {
    cleanup?.();
    cleanupSSEEventHandlers();
    eventHandlers.clear();
  });

  /**
   * AC #7: Test desktop sync_completed handler invalidates queries only on import with count > 0
   */
  describe("query invalidation logic", () => {
    it("invalidates queries on import with count > 0", () => {
      const { sseStore } = require("../stores/sseStore");

      const event = {
        type: "sync_completed",
        data: {
          channel: "tasks",
          direction: "import",
          count: 5,
          error: null,
        },
      };

      sseStore.getState().emit("sync_completed", event);

      expect(mockInvalidateQueries).toHaveBeenCalledWith({ queryKey: ["tasks"] });
      expect(mockInvalidateQueries).toHaveBeenCalledWith({ queryKey: ["epics"] });
      expect(mockInvalidateQueries).toHaveBeenCalledTimes(2);
    });

    it("does NOT invalidate queries on export direction (prevents feedback loop)", () => {
      const { sseStore } = require("../stores/sseStore");

      const event = {
        type: "sync_completed",
        data: {
          channel: "tasks",
          direction: "export",
          count: 3,
          error: null,
        },
      };

      sseStore.getState().emit("sync_completed", event);

      expect(mockInvalidateQueries).not.toHaveBeenCalled();
    });

    it("does NOT invalidate queries on import with count = 0", () => {
      const { sseStore } = require("../stores/sseStore");

      const event = {
        type: "sync_completed",
        data: {
          channel: "tasks",
          direction: "import",
          count: 0,
          error: null,
        },
      };

      sseStore.getState().emit("sync_completed", event);

      expect(mockInvalidateQueries).not.toHaveBeenCalled();
    });

    it("does NOT invalidate queries on import with undefined count", () => {
      const { sseStore } = require("../stores/sseStore");

      const event = {
        type: "sync_completed",
        data: {
          channel: "tasks",
          direction: "import",
          count: undefined,
          error: null,
        },
      };

      sseStore.getState().emit("sync_completed", event);

      expect(mockInvalidateQueries).not.toHaveBeenCalled();
    });

    it("handles sync_completed with error gracefully without invalidation", () => {
      const { sseStore } = require("../stores/sseStore");

      const event = {
        type: "sync_completed",
        data: {
          channel: "tasks",
          direction: "import",
          count: 0,
          error: "Connection failed",
        },
      };

      sseStore.getState().emit("sync_completed", event);

      expect(mockInvalidateQueries).not.toHaveBeenCalled();
    });

    it("invalidates queries for 'epics' channel on import with count > 0", () => {
      const { sseStore } = require("../stores/sseStore");

      const event = {
        type: "sync_completed",
        data: {
          channel: "epics",
          direction: "import",
          count: 2,
          error: null,
        },
      };

      sseStore.getState().emit("sync_completed", event);

      expect(mockInvalidateQueries).toHaveBeenCalledWith({ queryKey: ["tasks"] });
      expect(mockInvalidateQueries).toHaveBeenCalledWith({ queryKey: ["epics"] });
      expect(mockInvalidateQueries).toHaveBeenCalledTimes(2);
    });

    it("handles large count values correctly", () => {
      const { sseStore } = require("../stores/sseStore");

      const event = {
        type: "sync_completed",
        data: {
          channel: "tasks",
          direction: "import",
          count: 1000,
          error: null,
        },
      };

      sseStore.getState().emit("sync_completed", event);

      expect(mockInvalidateQueries).toHaveBeenCalledTimes(2);
    });

    it("handles negative count as falsy (no invalidation)", () => {
      const { sseStore } = require("../stores/sseStore");

      const event = {
        type: "sync_completed",
        data: {
          channel: "tasks",
          direction: "import",
          count: -1,
          error: null,
        },
      };

      sseStore.getState().emit("sync_completed", event);

      // Negative count should be falsy, so no invalidation
      expect(mockInvalidateQueries).not.toHaveBeenCalled();
    });
  });
});
