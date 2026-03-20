import { ConfirmButton } from "./ConfirmButton";
import { TaskIdLabel } from "./TaskIdLabel";
import { ErrorBoundary } from "./ErrorBoundary";
import { HealthCheckPanel } from "./HealthCheckPanel";
import type { VerificationRun, StepEntry } from "@/stores/verificationStore";

const meta = {
  title: "Shared/SmallComponents",
  parameters: {
    layout: "padded",
  },
};

export default meta;

// ── Helpers ──────────────────────────────────────────────────────────────────

function step(
  index: number,
  name: string,
  status: StepEntry["status"],
  overrides?: Partial<StepEntry>,
): StepEntry {
  return {
    index,
    name,
    phase: "verification",
    status,
    ...overrides,
  };
}

function ThrowError(): never {
  throw new Error("Test error");
}

// ── Mock verification runs ───────────────────────────────────────────────────

const passedRun: VerificationRun = {
  projectId: "proj-001",
  taskId: "019cbe9f-6ae7-7d90-a8be-6ba626cc0119",
  status: "passed",
  startedAt: "2026-03-19T10:30:00Z",
  steps: [
    step(0, "pnpm install", "passed", {
      phase: "setup",
      command: "pnpm install --frozen-lockfile",
      durationMs: 1_340,
      exitCode: 0,
      stdout:
        "Lockfile is up to date, resolution step is skipped\nDependencies are already up to date\nDone in 1.3s",
    }),
    step(1, "tsc --noEmit", "passed", {
      command: "pnpm tsc --noEmit",
      durationMs: 5_120,
      exitCode: 0,
      stdout: "Done in 5.1s",
    }),
    step(2, "vitest run", "passed", {
      command: "pnpm test",
      durationMs: 9_870,
      exitCode: 0,
      stdout:
        "Test Files  14 passed (14)\n Tests  53 passed (53)\n Duration  9.8s",
    }),
    step(3, "eslint", "passed", {
      command: "pnpm lint",
      durationMs: 3_210,
      exitCode: 0,
      stdout: "No ESLint warnings or errors\nDone in 3.2s",
    }),
  ],
};

const failedRun: VerificationRun = {
  projectId: "proj-001",
  taskId: "019cbe9f-6ae7-7d90-a8be-6ba626cc0119",
  status: "failed",
  startedAt: "2026-03-19T11:15:00Z",
  steps: [
    step(0, "pnpm install", "passed", {
      phase: "setup",
      command: "pnpm install --frozen-lockfile",
      durationMs: 1_120,
      exitCode: 0,
      stdout: "Already up to date\nDone in 1.1s",
    }),
    step(1, "tsc --noEmit", "failed", {
      command: "pnpm tsc --noEmit",
      durationMs: 4_600,
      exitCode: 2,
      stdout: "Found 3 errors in 2 files.",
      stderr: [
        "src/components/TaskCard.tsx(42,5): error TS2322: Type 'string' is not assignable to type 'number'.",
        "src/stores/sseStore.ts(18,3): error TS2741: Property 'reconnectDelay' is missing in type '{}' but required in type 'SSEConfig'.",
        "src/stores/sseStore.ts(25,7): error TS7006: Parameter 'evt' implicitly has an 'any' type.",
      ].join("\n"),
    }),
    step(2, "vitest run", "skipped"),
    step(3, "eslint", "skipped"),
  ],
};

const runningRun: VerificationRun = {
  projectId: "proj-001",
  status: "running",
  startedAt: "2026-03-19T12:00:00Z",
  steps: [
    step(0, "pnpm install", "passed", {
      phase: "setup",
      command: "pnpm install --frozen-lockfile",
      durationMs: 1_050,
      exitCode: 0,
      stdout: "Already up to date\nDone in 1.0s",
    }),
    step(1, "tsc --noEmit", "passed", {
      command: "pnpm tsc --noEmit",
      durationMs: 4_900,
      exitCode: 0,
      stdout: "Done in 4.9s",
    }),
    step(2, "vitest run", "running", {
      command: "pnpm test",
    }),
    step(3, "eslint", "skipped"),
  ],
};

// ── ConfirmButton ────────────────────────────────────────────────────────────

export const ConfirmButtonDefault = {
  render: () => (
    <ConfirmButton
      title="Delete task?"
      description="This action cannot be undone."
      onConfirm={() => {}}
    >
      Delete
    </ConfirmButton>
  ),
};

export const ConfirmButtonDisabled = {
  render: () => (
    <ConfirmButton
      title="Delete task?"
      description="This action cannot be undone."
      onConfirm={() => {}}
      disabled
    >
      Delete
    </ConfirmButton>
  ),
};

// ── TaskIdLabel ──────────────────────────────────────────────────────────────

export const TaskIdWithShortId = {
  render: () => (
    <TaskIdLabel
      taskId="019cbe9f-6ae7-7d90-a8be-6ba626cc0119"
      shortId="j4m1"
    />
  ),
};

export const TaskIdFullId = {
  render: () => (
    <TaskIdLabel taskId="019cbe9f-6ae7-7d90-a8be-6ba626cc0119" />
  ),
};

// ── ErrorBoundary ────────────────────────────────────────────────────────────

export const ErrorBoundaryTriggered = {
  render: () => (
    <ErrorBoundary>
      <ThrowError />
    </ErrorBoundary>
  ),
};

export const ErrorBoundaryNormal = {
  render: () => (
    <ErrorBoundary>
      <div className="p-4">Normal content renders fine</div>
    </ErrorBoundary>
  ),
};

// ── HealthCheckPanel ─────────────────────────────────────────────────────────

export const HealthCheckPassed = {
  render: () => (
    <HealthCheckPanel
      open={true}
      projectName="DjinnOS Desktop"
      run={passedRun}
      onClose={() => {}}
    />
  ),
  parameters: { layout: "fullscreen" },
};

export const HealthCheckFailed = {
  render: () => (
    <HealthCheckPanel
      open={true}
      projectName="DjinnOS Desktop"
      run={failedRun}
      onClose={() => {}}
    />
  ),
  parameters: { layout: "fullscreen" },
};

export const HealthCheckRunning = {
  render: () => (
    <HealthCheckPanel
      open={true}
      projectName="DjinnOS Desktop"
      run={runningRun}
      onClose={() => {}}
    />
  ),
  parameters: { layout: "fullscreen" },
};

export const HealthCheckNoRun = {
  render: () => (
    <HealthCheckPanel
      open={true}
      projectName="DjinnOS Desktop"
      run={null}
      onClose={() => {}}
    />
  ),
  parameters: { layout: "fullscreen" },
};
