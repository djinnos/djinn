import type { StepEntry } from "@/stores/verificationStore";
import { verificationStore } from "@/stores/verificationStore";

export type SetupVerificationView = {
  taskId: string;
  steps: StepEntry[];
  status: "running" | "passed" | "failed" | "cache_hit";
  hasData: boolean;
  allPassed: boolean;
  isRunning: boolean;
  hasFailed: boolean;
  totalDuration: number;
  failedStepId: string | null;
};

const EMPTY_STEPS: StepEntry[] = [];
export const EMPTY_SETUP_VERIFICATION: SetupVerificationView = {
  taskId: "",
  steps: EMPTY_STEPS,
  status: "passed",
  hasData: false,
  allPassed: false,
  isRunning: false,
  hasFailed: false,
  totalDuration: 0,
  failedStepId: null,
};

export function buildSetupVerificationView(taskId: string, state: ReturnType<typeof verificationStore.getState>): SetupVerificationView {
  const lifecycle = state.lifecycleSteps.get(taskId) ?? [];
  const run = Array.from(state.runs.values()).find((candidate) => candidate.taskId === taskId);

  if (lifecycle.length === 0 && (!run || run.steps.length === 0)) {
    return {
      ...EMPTY_SETUP_VERIFICATION,
      taskId,
    };
  }

  const mappedLifecycle: StepEntry[] = lifecycle.map((item, index) => ({
    index,
    name: item.detail ? `${item.step}: ${item.detail}` : item.step,
    phase: "setup",
    status: "passed",
    stdout: item.timestamp,
  }));

  const lifecycleOffset = mappedLifecycle.length;
  const verificationSteps: StepEntry[] = (run?.steps ?? []).map((step, index) => ({
    ...step,
    phase: "verification",
    index: lifecycleOffset + index,
  }));

  const steps = [...mappedLifecycle, ...verificationSteps];
  const isRunning = steps.some((step) => step.status === "running") || run?.status === "running";
  const hasFailed = steps.some((step) => step.status === "failed") || run?.status === "failed";
  const allPassed = steps.length > 0 && steps.every((step) => step.status === "passed") && !isRunning && !hasFailed;
  const totalDuration = steps.reduce((sum, step) => sum + (step.durationMs ?? 0), 0);
  const failedStep = steps.find((step) => step.status === "failed");
  const failedStepId = failedStep ? `step-${failedStep.index}` : null;
  const status = run?.status ?? (hasFailed ? "failed" : isRunning ? "running" : "passed");

  return {
    taskId,
    steps,
    status,
    hasData: steps.length > 0,
    allPassed,
    isRunning,
    hasFailed,
    totalDuration,
    failedStepId,
  };
}

export function areSetupVerificationViewsEqual(a: SetupVerificationView, b: SetupVerificationView): boolean {
  return (
    a.taskId === b.taskId &&
    a.status === b.status &&
    a.hasData === b.hasData &&
    a.allPassed === b.allPassed &&
    a.isRunning === b.isRunning &&
    a.hasFailed === b.hasFailed &&
    a.totalDuration === b.totalDuration &&
    a.failedStepId === b.failedStepId &&
    a.steps === b.steps
  );
}

