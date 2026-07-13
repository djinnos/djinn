import { useEffect } from 'react';
import { MemoryRouter } from 'react-router-dom';

import { EmptyState } from './EmptyState';
import { LoadingScreen } from './LoadingScreen';
import { InlineError } from './InlineError';
import { Sidebar } from './Sidebar';
import { ProjectSelector } from './ProjectSelector';
import { ConnectionStatus } from './ConnectionStatus';
import { ConfirmButton } from './ConfirmButton';
import { TaskIdLabel } from './TaskIdLabel';
import { ErrorBoundary } from './ErrorBoundary';
import { HealthCheckPanel } from './HealthCheckPanel';
import type { StepLogEntry, StepLogStatus } from './StepLog';

import type { Project } from '@/api/types';
import { useSidebarStore } from '@/stores/sidebarStore';
import { useProjectStore } from '@/stores/useProjectStore';
import { sseStore } from '@/stores/sseStore';

const withRouter = (Story: any) => (
  <MemoryRouter>
    <Story />
  </MemoryRouter>
);

const SidebarState = ({ section = 'tasks' }: { section?: 'tasks' | 'chat' | 'settings' }) => {
  const setActiveSection = useSidebarStore((s) => s.setActiveSection);

  useEffect(() => {
    setActiveSection(section);
  }, [section, setActiveSection]);

  return <Sidebar />;
};

const ProjectSelectorState = ({ selectedId }: { selectedId: string | null }) => {
  const setProjects = useProjectStore((s) => s.setProjects);
  const setSelectedProjectId = useProjectStore((s) => s.setSelectedProjectId);

  useEffect(() => {
    const projects = [
      { id: 'proj-1', name: 'DjinnOS Desktop', github_owner: 'djinnos', github_repo: 'desktop' },
      { id: 'proj-2', name: 'API Platform', github_owner: 'djinnos', github_repo: 'api' },
      { id: 'proj-3', name: 'Onboarding Improvements', github_owner: 'djinnos', github_repo: 'onboarding' },
    ] satisfies Project[];

    setProjects(projects);
    setSelectedProjectId(selectedId);
  }, [selectedId, setProjects, setSelectedProjectId]);

  return <ProjectSelector />;
};

const ConnectionStatusState = ({
  status,
  reconnectAttempt = 0,
}: {
  status: 'connected' | 'reconnecting' | 'error';
  reconnectAttempt?: number;
}) => {
  useEffect(() => {
    sseStore.getState().setConnectionStatus(status);
    const state = sseStore.getState();
    state.resetReconnectAttempt();
    for (let i = 0; i < reconnectAttempt; i += 1) {
      state.incrementReconnectAttempt();
    }
  }, [status, reconnectAttempt]);

  return <ConnectionStatus />;
};

export default {
  title: 'Shared/Components',
};

export const EmptyStateDefault = {
  name: 'EmptyState / Default',
  render: () => (
    <div className="h-[360px]">
      <EmptyState
        title="No tasks yet"
        message="Create your first task to start tracking work in this project."
        actionLabel="Create Task"
        onAction={() => {}}
      />
    </div>
  ),
};

export const EmptyStateCustomIllustration = {
  name: 'EmptyState / Custom Illustration',
  render: () => (
    <div className="h-[360px]">
      <EmptyState
        title="No epics found"
        message="Group related tasks by creating an epic."
        actionLabel="Add Epic"
        onAction={() => {}}
        illustration={<div className="text-4xl">📚</div>}
      />
    </div>
  ),
};

export const LoadingScreenLoading = {
  name: 'LoadingScreen / Loading',
  render: () => <LoadingScreen status="loading" message="Connecting to DjinnOS backend..." />,
};

export const LoadingScreenError = {
  name: 'LoadingScreen / Error',
  render: () => <LoadingScreen status="error" message="Unable to reach local server on port 4000." onRetry={() => {}} />,
};

export const LoadingScreenRetrying = {
  name: 'LoadingScreen / Retrying',
  render: () => <LoadingScreen status="error" message="Connection dropped. Retrying..." onRetry={() => {}} isRetrying />,
};

export const InlineErrorSimple = {
  name: 'InlineError / Message Only',
  render: () => <InlineError message="Failed to save changes." />,
};

export const InlineErrorWithRetry = {
  name: 'InlineError / With Retry',
  render: () => <InlineError message="Could not load projects." onRetry={() => {}} />,
};

export const InlineErrorRetrying = {
  name: 'InlineError / Retrying',
  render: () => <InlineError message="Temporary network issue." onRetry={() => {}} retrying />,
};

export const SidebarKanban = {
  name: 'Sidebar / Kanban',
  decorators: [withRouter],
  render: () => <SidebarState section="tasks" />,
};

export const SidebarSettings = {
  name: 'Sidebar / Settings',
  decorators: [withRouter],
  render: () => <SidebarState section="settings" />,
};

export const ProjectSelectorDefault = {
  name: 'ProjectSelector / Default',
  render: () => <ProjectSelectorState selectedId="proj-1" />,
};

export const ProjectSelectorDifferentSelection = {
  name: 'ProjectSelector / Different Selection',
  render: () => <ProjectSelectorState selectedId="proj-3" />,
};

export const ConnectionStatusConnected = {
  name: 'ConnectionStatus / Connected',
  render: () => <ConnectionStatusState status="connected" />,
};

export const ConnectionStatusReconnecting = {
  name: 'ConnectionStatus / Reconnecting',
  render: () => <ConnectionStatusState status="reconnecting" reconnectAttempt={2} />,
};

export const ConnectionStatusError = {
  name: 'ConnectionStatus / Error',
  render: () => <ConnectionStatusState status="error" />,
};

// ── Small components (merged from SmallComponents.stories.tsx) ───────────────

type HealthCheckRun = {
  status: StepLogStatus;
  startedAt?: string;
  steps: StepLogEntry[];
};

function step(
  index: number,
  name: string,
  status: StepLogEntry['status'],
  overrides?: Partial<StepLogEntry>,
): StepLogEntry {
  return {
    index,
    name,
    status,
    ...overrides,
  };
}

function ThrowError(): never {
  throw new Error('Test error');
}

const passedRun: HealthCheckRun = {
  status: 'passed',
  startedAt: '2026-03-19T10:30:00Z',
  steps: [
    step(0, 'pnpm install', 'passed', {
      command: 'pnpm install --frozen-lockfile',
      durationMs: 1_340,
      exitCode: 0,
      stdout:
        'Lockfile is up to date, resolution step is skipped\nDependencies are already up to date\nDone in 1.3s',
    }),
    step(1, 'tsc --noEmit', 'passed', {
      command: 'pnpm tsc --noEmit',
      durationMs: 5_120,
      exitCode: 0,
      stdout: 'Done in 5.1s',
    }),
    step(2, 'vitest run', 'passed', {
      command: 'pnpm test',
      durationMs: 9_870,
      exitCode: 0,
      stdout:
        'Test Files  14 passed (14)\n Tests  53 passed (53)\n Duration  9.8s',
    }),
    step(3, 'eslint', 'passed', {
      command: 'pnpm lint',
      durationMs: 3_210,
      exitCode: 0,
      stdout: 'No ESLint warnings or errors\nDone in 3.2s',
    }),
  ],
};

const failedRun: HealthCheckRun = {
  status: 'failed',
  startedAt: '2026-03-19T11:15:00Z',
  steps: [
    step(0, 'pnpm install', 'passed', {
      command: 'pnpm install --frozen-lockfile',
      durationMs: 1_120,
      exitCode: 0,
      stdout: 'Already up to date\nDone in 1.1s',
    }),
    step(1, 'tsc --noEmit', 'failed', {
      command: 'pnpm tsc --noEmit',
      durationMs: 4_600,
      exitCode: 2,
      stdout: 'Found 3 errors in 2 files.',
      stderr: [
        "src/components/TaskCard.tsx(42,5): error TS2322: Type 'string' is not assignable to type 'number'.",
        "src/stores/sseStore.ts(18,3): error TS2741: Property 'reconnectDelay' is missing in type '{}' but required in type 'SSEConfig'.",
        "src/stores/sseStore.ts(25,7): error TS7006: Parameter 'evt' implicitly has an 'any' type.",
      ].join('\n'),
    }),
    step(2, 'vitest run', 'skipped'),
    step(3, 'eslint', 'skipped'),
  ],
};

const runningRun: HealthCheckRun = {
  status: 'running',
  startedAt: '2026-03-19T12:00:00Z',
  steps: [
    step(0, 'pnpm install', 'passed', {
      command: 'pnpm install --frozen-lockfile',
      durationMs: 1_050,
      exitCode: 0,
      stdout: 'Already up to date\nDone in 1.0s',
    }),
    step(1, 'tsc --noEmit', 'passed', {
      command: 'pnpm tsc --noEmit',
      durationMs: 4_900,
      exitCode: 0,
      stdout: 'Done in 4.9s',
    }),
    step(2, 'vitest run', 'running', {
      command: 'pnpm test',
    }),
    step(3, 'eslint', 'skipped'),
  ],
};

export const ConfirmButtonDefault = {
  name: 'ConfirmButton / Default',
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
  name: 'ConfirmButton / Disabled',
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

export const TaskIdWithShortId = {
  name: 'TaskIdLabel / With Short ID',
  render: () => (
    <TaskIdLabel
      taskId="019cbe9f-6ae7-7d90-a8be-6ba626cc0119"
      shortId="j4m1"
    />
  ),
};

export const TaskIdFullId = {
  name: 'TaskIdLabel / Full ID',
  render: () => (
    <TaskIdLabel taskId="019cbe9f-6ae7-7d90-a8be-6ba626cc0119" />
  ),
};

export const ErrorBoundaryTriggered = {
  name: 'ErrorBoundary / Triggered',
  render: () => (
    <ErrorBoundary>
      <ThrowError />
    </ErrorBoundary>
  ),
};

export const ErrorBoundaryNormal = {
  name: 'ErrorBoundary / Normal',
  render: () => (
    <ErrorBoundary>
      <div className="p-4">Normal content renders fine</div>
    </ErrorBoundary>
  ),
};

export const HealthCheckPassed = {
  name: 'HealthCheckPanel / Passed',
  render: () => (
    <HealthCheckPanel
      open={true}
      projectName="DjinnOS Desktop"
      run={passedRun}
      onClose={() => {}}
    />
  ),
  parameters: { layout: 'fullscreen' },
};

export const HealthCheckFailed = {
  name: 'HealthCheckPanel / Failed',
  render: () => (
    <HealthCheckPanel
      open={true}
      projectName="DjinnOS Desktop"
      run={failedRun}
      onClose={() => {}}
    />
  ),
  parameters: { layout: 'fullscreen' },
};

export const HealthCheckRunning = {
  name: 'HealthCheckPanel / Running',
  render: () => (
    <HealthCheckPanel
      open={true}
      projectName="DjinnOS Desktop"
      run={runningRun}
      onClose={() => {}}
    />
  ),
  parameters: { layout: 'fullscreen' },
};

export const HealthCheckNoRun = {
  name: 'HealthCheckPanel / No Run',
  render: () => (
    <HealthCheckPanel
      open={true}
      projectName="DjinnOS Desktop"
      run={null}
      onClose={() => {}}
    />
  ),
  parameters: { layout: 'fullscreen' },
};
