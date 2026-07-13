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
import { DispatchPauseBanner } from "@/components/DispatchPauseBanner";
import type { DispatchPauseEntry } from "@/stores/dispatchPauseStore";

/* ---------------------------------------------------------------------------
 * BoardHealthBanner — presentational mock
 * Reproduces the exact JSX from BoardHealthBanner.tsx but accepts data as props
 * instead of relying on useBoardHealth().
 * --------------------------------------------------------------------------- */

interface LspWarning {
  server: string;
  message: string;
}

interface BoardHealthBannerMockProps {
  lspWarnings?: LspWarning[];
  projectIssues?: Record<string, string>;
}

function BoardHealthBannerMock({
  lspWarnings = [],
  projectIssues = {},
}: BoardHealthBannerMockProps) {
  const [dismissed, setDismissed] = useState(false);
  if (dismissed) return null;

  const issueEntries = Object.entries(projectIssues);
  const totalIssues =
    lspWarnings.length + issueEntries.length;

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
        </div>
      </CardContent>
    </Card>
  );
}

/* ---------------------------------------------------------------------------
 * Storybook meta & stories
 * --------------------------------------------------------------------------- */

const meta: Meta = {
  title: "Shared/Banners",
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

/* ---- DispatchPauseBanner stories ---- */

const dispatchPause = (
  scope: DispatchPauseEntry["scope"],
  targetId: string | null,
  reason: string,
  pausedBy = "ops-admin",
): DispatchPauseEntry => ({
  scope,
  target_id: targetId,
  paused_by: pausedBy,
  paused_at: "2026-01-01T12:00:00Z",
  reason,
});

export const DispatchPauseNoPause: StoryObj = {
  render: () => (
    <div className="rounded-md border border-dashed border-muted-foreground/40 p-4 text-sm text-muted-foreground">
      <DispatchPauseBanner
        entries={[]}
        selectedProjectId="project-alpha"
        currentUserId="user-1"
      />
      No pause banner is rendered for an empty status response.
    </div>
  ),
};

export const DispatchPauseGlobal: StoryObj = {
  render: () => (
    <DispatchPauseBanner
      entries={[dispatchPause("global", null, "Coordinator maintenance window")]}
      selectedProjectId="project-alpha"
      currentUserId="user-1"
    />
  ),
};

export const DispatchPauseProject: StoryObj = {
  render: () => (
    <DispatchPauseBanner
      entries={[dispatchPause("project", "project-alpha", "Repository migration")]}
      selectedProjectId="project-alpha"
      currentUserId="user-1"
    />
  ),
};

export const DispatchPauseUser: StoryObj = {
  render: () => (
    <DispatchPauseBanner
      entries={[
        dispatchPause("user", "user-1", "User dispatch temporarily held", "lead"),
      ]}
      selectedProjectId="unrelated-project"
      currentUserId="user-1"
    />
  ),
};

export const DispatchPauseMultipleSimultaneous: StoryObj = {
  render: () => (
    <DispatchPauseBanner
      entries={[
        dispatchPause("global", null, "Fleet safety check"),
        dispatchPause("project", "project-alpha", "Alpha dependency outage"),
        dispatchPause("user", "user-1", "Manual review of user queue", "pm"),
      ]}
      selectedProjectId="project-alpha"
      currentUserId="user-1"
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
