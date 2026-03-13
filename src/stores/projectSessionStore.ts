import { createStore } from "zustand/vanilla";

export interface ProjectSession {
  session_id?: string;
  project_id: string;
  agent_type: string;
  model_id: string;
  started_at: string;
  status: string;
}

export interface ProjectSessionState {
  activeProjectSessions: Map<string, ProjectSession>;
  upsertProjectSession: (session: ProjectSession) => void;
  removeProjectSession: (projectId: string) => void;
  getActiveProjectSession: (projectId: string) => ProjectSession | undefined;
  clearProjectSessions: () => void;
}

export const projectSessionStore = createStore<ProjectSessionState>((set, get) => ({
  activeProjectSessions: new Map(),

  upsertProjectSession: (session) =>
    set((state) => {
      const next = new Map(state.activeProjectSessions);
      next.set(session.project_id, session);
      return { activeProjectSessions: next };
    }),

  removeProjectSession: (projectId) =>
    set((state) => {
      if (!state.activeProjectSessions.has(projectId)) return state;
      const next = new Map(state.activeProjectSessions);
      next.delete(projectId);
      return { activeProjectSessions: next };
    }),

  getActiveProjectSession: (projectId) => get().activeProjectSessions.get(projectId),

  clearProjectSessions: () => set({ activeProjectSessions: new Map() }),
}));
