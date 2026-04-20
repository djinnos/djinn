import { createStore } from "zustand/vanilla";

export interface StepEntry {
  index: number;
  name: string;
  command?: string;
  phase: "setup" | "verification";
  status: "running" | "passed" | "failed" | "skipped";
  exitCode?: number;
  durationMs?: number;
  stdout?: string;
  stderr?: string;
}

export interface VerificationRun {
  projectId: string;
  taskId?: string;
  steps: StepEntry[];
  status: "running" | "passed" | "failed" | "cache_hit";
  startedAt: string;
}

export interface LifecycleStep {
  step: string;
  detail?: string;
  timestamp: string;
}

interface VerificationState {
  runs: Map<string, VerificationRun>;
  lifecycleSteps: Map<string, LifecycleStep[]>;

  addStep: (key: string, step: StepEntry, meta: { projectId: string; taskId?: string; startedAt?: string }) => void;
  updateStep: (key: string, index: number, update: Partial<StepEntry>) => void;
  setRunStatus: (key: string, status: VerificationRun["status"]) => void;
  clearRun: (key: string) => void;

  addLifecycleStep: (taskId: string, step: LifecycleStep) => void;
  clearLifecycleSteps: (taskId: string) => void;
}

export const verificationStore = createStore<VerificationState>((set, get) => ({
  runs: new Map(),
  lifecycleSteps: new Map(),

  addStep: (key, step, meta) => {
    const runs = new Map(get().runs);
    const existing = runs.get(key);

    if (!existing) {
      runs.set(key, {
        projectId: meta.projectId,
        taskId: meta.taskId,
        steps: [step],
        status: "running",
        startedAt: meta.startedAt ?? new Date().toISOString(),
      });
    } else {
      runs.set(key, {
        ...existing,
        steps: [...existing.steps, step],
      });
    }

    set({ runs });
  },

  updateStep: (key, index, update) => {
    const runs = new Map(get().runs);
    const existing = runs.get(key);
    if (!existing) return;

    const steps = existing.steps.map((step) => (step.index === index ? { ...step, ...update } : step));
    runs.set(key, { ...existing, steps });
    set({ runs });
  },

  setRunStatus: (key, status) => {
    const runs = new Map(get().runs);
    const existing = runs.get(key);
    if (!existing) return;
    runs.set(key, { ...existing, status });
    set({ runs });
  },

  clearRun: (key) => {
    const runs = new Map(get().runs);
    runs.delete(key);
    set({ runs });
  },

  addLifecycleStep: (taskId, step) => {
    const lifecycleSteps = new Map(get().lifecycleSteps);
    const existing = lifecycleSteps.get(taskId) ?? [];
    lifecycleSteps.set(taskId, [...existing, step]);
    set({ lifecycleSteps });
  },

  clearLifecycleSteps: (taskId) => {
    const lifecycleSteps = new Map(get().lifecycleSteps);
    lifecycleSteps.delete(taskId);
    set({ lifecycleSteps });
  },
}));
