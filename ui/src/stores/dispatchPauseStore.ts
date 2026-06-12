import { createStore } from "zustand/vanilla";
import { useStoreWithSelector } from "./useStoreWithSelector";
import {
  fetchDispatchPauseStatus,
  type DispatchPauseMetadata,
  type DispatchPauseScope,
  type DispatchPauseStatusOutput,
} from "@/api/dispatchPause";

export type { DispatchPauseMetadata, DispatchPauseScope } from "@/api/dispatchPause";

export interface DispatchPauseEntry extends DispatchPauseMetadata {
  scope: DispatchPauseScope;
  target_id: string | null;
}

export interface DispatchPauseSsePayload {
  scope?: DispatchPauseScope | null;
  target_id?: string | null;
  current?: DispatchPauseMetadata | null;
  previous?: DispatchPauseMetadata | null;
  paused_by?: string | null;
  resumed_by?: string | null;
  actor?: string | null;
  changed_at?: string | null;
  reason?: string | null;
}

export interface DispatchPauseState {
  global: DispatchPauseEntry | null;
  projects: Record<string, DispatchPauseEntry>;
  users: Record<string, DispatchPauseEntry>;
  isHydrating: boolean;
  lastHydratedAt: number | null;
  lastError: Error | null;
  setFromStatusResponse: (response: DispatchPauseStatusOutput) => void;
  applySsePayload: (payload: DispatchPauseSsePayload) => void;
  upsert: (entry: DispatchPauseEntry) => void;
  clearScope: (scope: DispatchPauseScope, targetId?: string | null) => void;
  clearAll: () => void;
  getEntry: (scope: DispatchPauseScope, targetId?: string | null) => DispatchPauseEntry | null;
  getEntriesForScopes: (scopes: Array<{ scope: DispatchPauseScope; target_id?: string | null }>) => DispatchPauseEntry[];
  getAffectedEntries: (projectIds?: Array<string | null | undefined>, userId?: string | null) => DispatchPauseEntry[];
  hydrate: () => Promise<void>;
}

function entry(scope: DispatchPauseScope, targetId: string | null, pause: DispatchPauseMetadata): DispatchPauseEntry {
  return { ...pause, scope, target_id: targetId };
}

function normalizeTargetId(targetId?: string | null): string | null {
  if (typeof targetId !== "string") return null;
  const trimmed = targetId.trim();
  return trimmed.length > 0 ? trimmed : null;
}

function entriesFromStatus(response: DispatchPauseStatusOutput): Pick<DispatchPauseState, "global" | "projects" | "users"> {
  const next: Pick<DispatchPauseState, "global" | "projects" | "users"> = {
    global: null,
    projects: {},
    users: {},
  };

  if (response.state) {
    if (response.state.global) {
      next.global = entry("global", null, response.state.global);
    }
    for (const [targetId, pause] of Object.entries(response.state.projects ?? {})) {
      next.projects[targetId] = entry("project", targetId, pause);
    }
    for (const [targetId, pause] of Object.entries(response.state.users ?? {})) {
      next.users[targetId] = entry("user", targetId, pause);
    }
    return next;
  }

  if (response.scope === "global") {
    next.global = response.current ? entry("global", null, response.current) : null;
  } else if (response.scope === "project") {
    const targetId = normalizeTargetId(response.target_id);
    if (targetId && response.current) {
      next.projects[targetId] = entry("project", targetId, response.current);
    }
  } else if (response.scope === "user") {
    const targetId = normalizeTargetId(response.target_id);
    if (targetId && response.current) {
      next.users[targetId] = entry("user", targetId, response.current);
    }
  }

  return next;
}

function uniqueEntries(entries: DispatchPauseEntry[]): DispatchPauseEntry[] {
  const seen = new Set<string>();
  return entries.filter((candidate) => {
    const key = `${candidate.scope}:${candidate.target_id ?? ""}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

export const dispatchPauseStore = createStore<DispatchPauseState>((set, get) => ({
  global: null,
  projects: {},
  users: {},
  isHydrating: false,
  lastHydratedAt: null,
  lastError: null,

  setFromStatusResponse: (response) => {
    const next = entriesFromStatus(response);
    set({ ...next, lastHydratedAt: Date.now(), lastError: null });
  },

  applySsePayload: (payload) => {
    const scope = payload.scope ?? undefined;
    if (!scope) return;
    const targetId = normalizeTargetId(payload.target_id);

    if (payload.current) {
      get().upsert(entry(scope, scope === "global" ? null : targetId, payload.current));
      return;
    }

    get().clearScope(scope, targetId);
  },

  upsert: (pauseEntry) => {
    if (pauseEntry.scope === "global") {
      set({ global: { ...pauseEntry, target_id: null } });
      return;
    }

    const targetId = normalizeTargetId(pauseEntry.target_id);
    if (!targetId) return;

    if (pauseEntry.scope === "project") {
      set((state) => ({
        projects: { ...state.projects, [targetId]: { ...pauseEntry, target_id: targetId } },
      }));
      return;
    }

    set((state) => ({
      users: { ...state.users, [targetId]: { ...pauseEntry, target_id: targetId } },
    }));
  },

  clearScope: (scope, targetId) => {
    if (scope === "global") {
      set({ global: null });
      return;
    }

    const normalized = normalizeTargetId(targetId);
    if (!normalized) return;

    if (scope === "project") {
      set((state) => {
        const projects = { ...state.projects };
        delete projects[normalized];
        return { projects };
      });
      return;
    }

    set((state) => {
      const users = { ...state.users };
      delete users[normalized];
      return { users };
    });
  },

  clearAll: () => set({ global: null, projects: {}, users: {}, lastError: null }),

  getEntry: (scope, targetId) => {
    const state = get();
    if (scope === "global") return state.global;
    const normalized = normalizeTargetId(targetId);
    if (!normalized) return null;
    return scope === "project" ? state.projects[normalized] ?? null : state.users[normalized] ?? null;
  },

  getEntriesForScopes: (scopes) => {
    const state = get();
    return uniqueEntries(
      scopes.flatMap(({ scope, target_id }) => {
        if (scope === "global") return state.global ? [state.global] : [];
        const normalized = normalizeTargetId(target_id);
        if (!normalized) return [];
        const candidate = scope === "project" ? state.projects[normalized] : state.users[normalized];
        return candidate ? [candidate] : [];
      }),
    );
  },

  getAffectedEntries: (projectIds = [], userId = null) => {
    const state = get();
    const affected: DispatchPauseEntry[] = [];
    if (state.global) affected.push(state.global);

    for (const projectId of projectIds) {
      const normalized = normalizeTargetId(projectId);
      if (normalized && state.projects[normalized]) affected.push(state.projects[normalized]);
    }

    const normalizedUserId = normalizeTargetId(userId);
    if (normalizedUserId && state.users[normalizedUserId]) affected.push(state.users[normalizedUserId]);

    return uniqueEntries(affected);
  },

  hydrate: async () => {
    set({ isHydrating: true, lastError: null });
    try {
      const response = await fetchDispatchPauseStatus();
      get().setFromStatusResponse(response);
    } catch (error) {
      set({ lastError: error instanceof Error ? error : new Error(String(error)) });
    } finally {
      set({ isHydrating: false });
    }
  },
}));

export function refreshDispatchPauseStatus(): Promise<void> {
  return dispatchPauseStore.getState().hydrate();
}

export function applyDispatchPauseSsePayload(payload: DispatchPauseSsePayload): void {
  dispatchPauseStore.getState().applySsePayload(payload);
}

export function useDispatchPauseStore(): DispatchPauseState;
export function useDispatchPauseStore<TSelected>(selector: (state: DispatchPauseState) => TSelected): TSelected;
export function useDispatchPauseStore<TSelected>(selector?: (state: DispatchPauseState) => TSelected): DispatchPauseState | TSelected {
  return useStoreWithSelector(dispatchPauseStore, selector);
}
