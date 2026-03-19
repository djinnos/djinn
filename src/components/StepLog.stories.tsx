import { StepLog } from "./StepLog";
import type { StepEntry } from "@/stores/verificationStore";

export default {
  title: "Components/StepLog",
  component: StepLog,
  decorators: [
    (Story: React.ComponentType) => (
      <div className="mx-auto max-w-2xl p-6 bg-background text-foreground">
        <Story />
      </div>
    ),
  ],
};

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

// ── Stories ──────────────────────────────────────────────────────────────────

export const AllPassed = {
  args: {
    steps: [
      step(0, "pnpm install", "passed", {
        phase: "setup",
        command: "pnpm install --frozen-lockfile",
        durationMs: 1_230,
        exitCode: 0,
        stdout: "Lockfile is up to date, resolution step is skipped\nDependencies are already up to date\nDone in 1.2s",
      }),
      step(1, "pnpm tsc --noEmit", "passed", {
        command: "pnpm tsc --noEmit",
        durationMs: 4_820,
        exitCode: 0,
        stdout: "Done in 4.8s",
      }),
      step(2, "pnpm lint", "passed", {
        command: "pnpm lint",
        durationMs: 3_150,
        exitCode: 0,
        stdout: "No ESLint warnings or errors\nDone in 3.1s",
      }),
      step(3, "pnpm test", "passed", {
        command: "pnpm test",
        durationMs: 8_940,
        exitCode: 0,
        stdout: "Test Files  12 passed (12)\n Tests  47 passed (47)\n Duration  8.9s",
      }),
    ],
    status: "passed",
  },
};

export const WithFailure = {
  args: {
    steps: [
      step(0, "pnpm tsc --noEmit", "passed", {
        command: "pnpm tsc --noEmit",
        durationMs: 4_700,
        exitCode: 0,
        stdout: "Done in 4.7s",
      }),
      step(1, "pnpm lint", "passed", {
        command: "pnpm lint",
        durationMs: 2_900,
        exitCode: 0,
        stdout: "No ESLint warnings or errors",
      }),
      step(2, "pnpm test", "failed", {
        command: "pnpm test",
        durationMs: 6_200,
        exitCode: 1,
        stdout: "Test Files  1 failed | 11 passed (12)\n Tests  1 failed | 46 passed (47)\n Duration  6.2s",
        stderr: [
          "FAIL src/stores/sseStore.test.ts > SSE reconnection > reconnects with Last-Event-ID header",
          "",
          "AssertionError: expected undefined to be '42'",
          "",
          "  - Expected: '42'",
          "  + Received: undefined",
          "",
          "  at src/stores/sseStore.test.ts:87:31",
        ].join("\n"),
      }),
    ],
    status: "failed",
    emphasizedStepId: "step-2",
  },
};

export const Running = {
  args: {
    steps: [
      step(0, "pnpm install", "passed", {
        phase: "setup",
        command: "pnpm install --frozen-lockfile",
        durationMs: 1_100,
        exitCode: 0,
        stdout: "Already up to date\nDone in 1.1s",
      }),
      step(1, "pnpm tsc --noEmit", "passed", {
        command: "pnpm tsc --noEmit",
        durationMs: 4_500,
        exitCode: 0,
        stdout: "Done in 4.5s",
      }),
      step(2, "pnpm lint", "running", {
        command: "pnpm lint",
      }),
      step(3, "pnpm test", "skipped"),
    ],
    status: "running",
  },
};

export const CacheHit = {
  args: {
    steps: [
      step(0, "pnpm tsc --noEmit", "passed", {
        command: "pnpm tsc --noEmit",
        durationMs: 4_800,
        exitCode: 0,
      }),
      step(1, "pnpm lint", "passed", {
        command: "pnpm lint",
        durationMs: 3_000,
        exitCode: 0,
      }),
      step(2, "pnpm test", "passed", {
        command: "pnpm test",
        durationMs: 8_200,
        exitCode: 0,
      }),
    ],
    status: "cache_hit",
    originalDurationMs: 16_000,
  },
};

export const Empty = {
  args: {
    steps: [],
    status: "running",
  },
};
