import { useEffect } from 'react';
import { MemoryRouter } from 'react-router-dom';

import { EmptyState } from './EmptyState';
import { LoadingScreen } from './LoadingScreen';
import { InlineError } from './InlineError';
import { Sidebar } from './Sidebar';
import { ProjectSelector } from './ProjectSelector';
import { ConnectionStatus } from './ConnectionStatus';

import type { Project } from '@/api/types';
import { useSidebarStore } from '@/stores/sidebarStore';
import { useProjectStore } from '@/stores/useProjectStore';
import { sseStore } from '@/stores/sseStore';

const withRouter = (Story: any) => (
  <MemoryRouter>
    <Story />
  </MemoryRouter>
);

const SidebarState = ({ section = 'kanban' }: { section?: 'kanban' | 'chat' | 'settings' }) => {
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
  render: () => <SidebarState section="kanban" />,
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
