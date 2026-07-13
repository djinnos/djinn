/**
 * Storybook mock for `@/hooks/useSessionMessages`.
 *
 * The real hook fetches historical messages and subscribes to live SSE deltas,
 * which can't run in Storybook. `vi.mock(...)` — how `TaskSessionPage.stories`
 * used to replace it — crashes under `storybook dev` (Vite builder), so
 * `.storybook/main.ts` aliases this module in at bundle time instead.
 *
 * The alias is GLOBAL, but only `TaskSessionPage` actually CALLS the hook (all
 * other importers pull types only, which resolve to the real module via
 * tsconfig paths and are erased at runtime). By default it returns an empty,
 * settled result. A story installs its fixture via `setSessionMessages` in a
 * `beforeEach`; module state resets on the full page reload Storybook does when
 * navigating between stories.
 */
import type { useSessionMessages as RealUseSessionMessages } from "../hooks/useSessionMessages";

type Result = ReturnType<typeof RealUseSessionMessages>;

const emptyResult: Result = {
  timeline: [],
  sessions: [],
  loading: false,
  error: null,
  streamingText: new Map<string, string>(),
  streamingThinking: new Map<string, string>(),
  refetch: async () => {},
};

let current: Result = emptyResult;

/** Install the timeline/session fixture for the current story. */
export function setSessionMessages(next: Partial<Result>): void {
  current = { ...emptyResult, ...next };
}

/** Restore the default empty result. */
export function resetSessionMessages(): void {
  current = emptyResult;
}

// The real hook takes `(taskId, projectSlug)`; the mock ignores both (extra
// positional args at the call site are harmless) and returns the staged result.
export function useSessionMessages(): Result {
  return current;
}
