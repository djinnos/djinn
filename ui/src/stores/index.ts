/**
 * Stores index - Export all store modules
 */

// Chat Store
export { useChatStore, type ChatState, type ChatSession, type ChatMessage } from "./chatStore";

// SSE Store
export { sseStore, type SSEState, type SSEEvent, type SSEEventType } from "./sseStore";

// Task Store
export { taskStore, type TaskState } from "./taskStore";
export {
  useTaskStore,
  useTask,
  useTasksByEpic,
  useTasksByStatus,
  useAllTasks,
  useTaskCount,
} from "./useTaskStore";

// Epic Store
export { epicStore, type EpicState } from "./epicStore";
export {
  useEpicStore,
  useEpic,
  useEpicsByStatus,
  useAllEpics,
  useEpicCount,
} from "./useEpicStore";

// SSE Event Handlers
export { initSSEEventHandlers, cleanupSSEEventHandlers } from "./sseEventHandlers";

// Base hook
export { useStoreWithSelector } from "./useStoreWithSelector";
