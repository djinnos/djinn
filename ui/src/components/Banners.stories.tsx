import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { Card, CardContent } from "@/components/ui/card";
import {
  Alert02Icon,
  Cancel01Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Button } from "@/components/ui/button";
import { Loading02Icon } from "@hugeicons/core-free-icons";

/* ---------------------------------------------------------------------------
 * BoardHealthBanner — presentational mock
 * Reproduces the exact JSX from BoardHealthBanner.tsx but accepts data as props
 * instead of relying on useBoardHealth() / verificationStore.
 * --------------------------------------------------------------------------- */

interface LspWarning {
  server: string;
  message: string;
}

interface StepEntryMock {
  index: number;
  name: string;
  command?: string;
  phase: "setup" | "verification";
  status: "running" | "passed" | "failed" | "skipped";
  exitCode?: number;
  stderr?: string;
}

interface BoardHealthBannerMockProps {
  lspWarnings?: LspWarning[];
  projectIssues?: Record<string, string>;
  failedSteps?: StepEntryMock[];
  failedRunTaskId?: string;
}

function BoardHealthBannerMock({
  lspWarnings = [],
  projectIssues = {},
  failedSteps = [],
  failedRunTaskId,
}: BoardHealthBannerMockProps) {
  const [dismissed, setDismissed] = useState(false);
  if (dismissed) return null;

  const issueEntries = Object.entries(projectIssues);
  const totalIssues =
    lspWarnings.length + issueEntries.length + failedSteps.length;

  return (
    <Card className="mx-4 border-amber-500/20 bg-amber-500/[0.04]">
      <CardContent className="py-3">
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-start gap-2.5">
            <div className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-amber-500/15">
              <HugeiconsIcon
                icon={Alert02Icon}
                className="size-3.5 text-amber-400"
              />
            </div>
            <span className="text-sm font-medium text-amber-200">
              {totalIssues} health{" "}
              {totalIssues === 1 ? "issue" : "issues"}
            </span>
          </div>
          <button
            type="button"
            aria-label="Dismiss board health banner"
            onClick={() => setDismissed(true)}
            className="shrink-0 rounded-md p-0.5 text-muted-foreground transition-colors hover:bg-muted/40 hover:text-foreground"
          >
            <HugeiconsIcon icon={Cancel01Icon} className="size-3.5" />
          </button>
        </div>

        <div className="mt-2 flex flex-col gap-1.5 pl-8.5">
          {/* Project health issues */}
          {issueEntries.map(([projectId, message]) => (
            <div
              key={projectId}
              className="flex items-start gap-2 text-xs text-red-400"
            >
              <span className="mt-px shrink-0 font-medium">project:</span>
              <span className="text-red-300/80">{message}</span>
            </div>
          ))}

          {/* LSP warnings */}
          {lspWarnings.map((w) => (
            <div
              key={w.server}
              className="flex items-start gap-2 text-xs text-amber-300/80"
            >
              <span className="mt-px shrink-0 font-medium text-amber-400">
                {w.server}:
              </span>
              <span>{w.message}</span>
            </div>
          ))}

          {/* Failed verification/setup steps */}
          {failedSteps.map((step) => (
            <div
              key={`${step.phase}-${step.index}`}
              className="flex items-start gap-2 text-xs text-red-400"
            >
              <span className="mt-px shrink-0 font-medium">
                {step.phase === "setup" ? "setup" : "verify"} failed:
              </span>
              <span className="text-red-300/80">
                {step.name}
                {step.command ? (
                  <code className="ml-1.5 rounded bg-white/5 px-1 py-0.5 font-mono text-[10px]">
                    {step.command}
                  </code>
                ) : null}
                {step.exitCode != null ? (
                  <span className="ml-1 text-muted-foreground">
                    (exit {step.exitCode})
                  </span>
                ) : null}
              </span>
            </div>
          ))}

          {/* Show stderr for the first failed step if available */}
          {failedSteps.length > 0 && failedSteps[0].stderr && (
            <pre className="mt-1 max-h-24 overflow-auto rounded bg-black/30 p-2 font-mono text-[10px] leading-relaxed text-red-300/70">
              {failedSteps[0].stderr.trim().slice(0, 500)}
            </pre>
          )}

          {/* Show which task failed if it was task-scoped */}
          {failedRunTaskId && (
            <span className="text-[10px] text-muted-foreground">
              task: {failedRunTaskId}
            </span>
          )}
        </div>
      </CardContent>
    </Card>
  );
}

/* ---------------------------------------------------------------------------
 * Storybook meta & stories
 * --------------------------------------------------------------------------- */

const meta: Meta = {
  title: "Banners",
  parameters: {
    layout: "padded",
  },
  decorators: [
    (Story: () => React.JSX.Element) => (
      <div className="max-w-2xl">
        <Story />
      </div>
    ),
  ],
};
export default meta;

/* ---- BoardHealthBanner stories ---- */

export const LspWarnings: StoryObj = {
  render: () => (
    <BoardHealthBannerMock
      lspWarnings={[
        { server: "typescript", message: "TypeScript server not responding" },
        { server: "eslint", message: "ESLint server disconnected" },
      ]}
    />
  ),
};

export const ProjectIssues: StoryObj = {
  render: () => (
    <BoardHealthBannerMock
      projectIssues={{
        "/home/user/projects/webapp": "Missing package.json — cannot resolve dependencies",
      }}
    />
  ),
};

export const FailedVerification: StoryObj = {
  render: () => (
    <BoardHealthBannerMock
      failedSteps={[
        {
          index: 0,
          name: "Install dependencies",
          command: "pnpm install --frozen-lockfile",
          phase: "setup",
          status: "failed",
          exitCode: 1,
          stderr:
            "ERR_PNPM_FROZEN_LOCKFILE  Cannot perform installation with frozen lockfile because the lockfile needs updates.\n\nNote: If you are running this command in CI, make sure that pnpm-lock.yaml is up to date.",
        },
      ]}
      failedRunTaskId="019cbe9f-6ae7-7d90-a8be-6ba626cc0119"
    />
  ),
};

export const MultipleIssues: StoryObj = {
  render: () => (
    <BoardHealthBannerMock
      lspWarnings={[
        { server: "typescript", message: "TypeScript server not responding" },
      ]}
      projectIssues={{
        "/home/user/projects/api": "Git working tree is dirty — uncommitted changes detected",
      }}
      failedSteps={[
        {
          index: 2,
          name: "Type check",
          command: "pnpm tsc --noEmit",
          phase: "verification",
          status: "failed",
          exitCode: 2,
          stderr: "src/index.ts(14,5): error TS2322: Type 'string' is not assignable to type 'number'.",
        },
      ]}
    />
  ),
};

export const BoardHealthMultipleFailures: StoryObj = {
  render: () => (
    <BoardHealthBannerMock
      failedSteps={[
        {
          index: 0,
          name: "Install dependencies",
          command: "pnpm install --frozen-lockfile",
          phase: "setup",
          status: "failed",
          exitCode: 1,
          stderr:
            "ERR_PNPM_FROZEN_LOCKFILE  Cannot perform installation with frozen lockfile because the lockfile needs updates.",
        },
        {
          index: 1,
          name: "Type check",
          command: "pnpm tsc --noEmit",
          phase: "verification",
          status: "failed",
          exitCode: 2,
          stderr:
            "src/index.ts(14,5): error TS2322: Type 'string' is not assignable to type 'number'.\nsrc/api/client.ts(88,12): error TS2345: Argument of type 'null' is not assignable to parameter of type 'Request'.",
        },
        {
          index: 2,
          name: "Lint",
          command: "pnpm lint",
          phase: "verification",
          status: "failed",
          exitCode: 1,
        },
      ]}
      failedRunTaskId="019cbe9f-6ae7-7d90-a8be-6ba626cc0119"
    />
  ),
};

/* ---- GitHubAppBanner stories ---- */

function GitHubAppBannerMock() {
  const [dismissed, setDismissed] = useState(false);
  if (dismissed) return null;

  return (
    <Card className="mx-4 border-amber-500/30 bg-amber-500/10">
      <CardContent className="py-4">
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-start gap-3">
            <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-amber-500/20">
              <HugeiconsIcon
                icon={Alert02Icon}
                className="size-4 text-amber-400"
              />
            </div>
            <div className="flex flex-col gap-1">
              <h3 className="text-sm font-semibold text-amber-200">
                GitHub App Not Installed
              </h3>
              <p className="text-sm text-muted-foreground">
                Install the Djinn app on your GitHub organization to enable PR
                creation and review feedback.
              </p>
            </div>
          </div>
          <button
            type="button"
            aria-label="Dismiss GitHub App banner"
            onClick={() => setDismissed(true)}
            className="shrink-0 rounded-md p-1 text-muted-foreground transition-colors hover:bg-muted/40 hover:text-foreground"
          >
            <HugeiconsIcon icon={Cancel01Icon} className="size-4" />
          </button>
        </div>

        <div className="mt-3 flex items-center gap-2 pl-11">
          <Button
            variant="outline"
            size="sm"
            className="h-7 gap-1.5 px-3 text-xs"
            onClick={() =>
              window.open(
                "https://github.com/apps/djinn-ai-bot/installations/new",
                "_blank"
              )
            }
          >
            Install on GitHub
          </Button>
          <Button
            variant="ghost"
            size="sm"
            className="h-7 gap-1.5 px-3 text-xs"
            disabled={false}
          >
            Check again
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}

export const GitHubAppNotInstalled: StoryObj = {
  render: () => <GitHubAppBannerMock />,
};

export const GitHubAppChecking: StoryObj = {
  render: () => (
    <Card className="mx-4 border-amber-500/30 bg-amber-500/10">
      <CardContent className="py-4">
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-start gap-3">
            <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-amber-500/20">
              <HugeiconsIcon
                icon={Alert02Icon}
                className="size-4 text-amber-400"
              />
            </div>
            <div className="flex flex-col gap-1">
              <h3 className="text-sm font-semibold text-amber-200">
                GitHub App Not Installed
              </h3>
              <p className="text-sm text-muted-foreground">
                Install the Djinn app on your GitHub organization to enable PR
                creation and review feedback.
              </p>
            </div>
          </div>
          <button
            type="button"
            aria-label="Dismiss GitHub App banner"
            className="shrink-0 rounded-md p-1 text-muted-foreground transition-colors hover:bg-muted/40 hover:text-foreground"
          >
            <HugeiconsIcon icon={Cancel01Icon} className="size-4" />
          </button>
        </div>

        <div className="mt-3 flex items-center gap-2 pl-11">
          <Button
            variant="outline"
            size="sm"
            className="h-7 gap-1.5 px-3 text-xs"
          >
            Install on GitHub
          </Button>
          <Button
            variant="ghost"
            size="sm"
            className="h-7 gap-1.5 px-3 text-xs"
            disabled
          >
            <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin" />
          </Button>
        </div>
      </CardContent>
    </Card>
  ),
};
