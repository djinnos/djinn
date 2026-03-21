import { useEffect } from 'react';
import { MemoryRouter } from 'react-router-dom';

import { EmptyState } from './EmptyState';
import { LoadingScreen } from './LoadingScreen';
import { InlineError } from './InlineError';
import { WizardStepIndicator } from './WizardStepIndicator';
import { WizardStep } from './WizardStep';
import { Sidebar } from './Sidebar';
import { ProjectSelector } from './ProjectSelector';
import { ConnectionStatus } from './ConnectionStatus';
import { Wizard } from './Wizard';

import type { Project } from '@/api/types';
import { useSidebarStore } from '@/stores/sidebarStore';
import { useProjectStore } from '@/stores/useProjectStore';
import { sseStore } from '@/stores/sseStore';
import { useWizardStore } from '@/stores/wizardStore';

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
      { id: 'proj-1', name: 'DjinnOS Desktop', path: '/workspace/djinnos-desktop' },
      { id: 'proj-2', name: 'API Platform', path: '/workspace/api-platform' },
      { id: 'proj-3', name: 'Onboarding Improvements', path: '/workspace/onboarding-improvements' },
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

const WizardState = ({ currentStep = 1, totalSteps = 4, completedSteps = [], skippedSteps = [] }: { currentStep?: number; totalSteps?: number; completedSteps?: number[]; skippedSteps?: number[] }) => {
  const resetWizard = useWizardStore((s) => s.resetWizard);
  const goToStep = useWizardStore((s) => s.goToStep);
  const markStepComplete = useWizardStore((s) => s.markStepComplete);

  useEffect(() => {
    resetWizard();
    goToStep(currentStep);
    completedSteps.forEach((step) => markStepComplete(step));
    if (skippedSteps.length > 0) {
      useWizardStore.setState({ skippedSteps });
    }
    if (totalSteps !== useWizardStore.getState().totalSteps) {
      useWizardStore.setState({ totalSteps });
    }
  }, [
    completedSteps,
    currentStep,
    goToStep,
    markStepComplete,
    resetWizard,
    skippedSteps,
    totalSteps,
  ]);

  return (
    <Wizard>
      <div className="space-y-3 rounded-md border p-4">
        <h3 className="font-semibold">Welcome to DjinnOS</h3>
        <p className="text-sm text-muted-foreground">Configure your workspace and preferences to get started.</p>
      </div>
    </Wizard>
  );
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

export const WizardStepIndicatorInitial = {
  name: 'WizardStepIndicator / Initial',
  render: () => <WizardStepIndicator currentStep={1} totalSteps={4} completedSteps={[]} skippedSteps={[]} />,
};

export const WizardStepIndicatorProgress = {
  name: 'WizardStepIndicator / Progress',
  render: () => <WizardStepIndicator currentStep={3} totalSteps={5} completedSteps={[1, 2]} skippedSteps={[]} />,
};

export const WizardStepIndicatorWithSkipped = {
  name: 'WizardStepIndicator / With Skipped Step',
  render: () => <WizardStepIndicator currentStep={4} totalSteps={5} completedSteps={[1, 2, 3]} skippedSteps={[2]} />,
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

export const WizardFlowInitial = {
  name: 'Wizard / Initial Step',
  render: () => <WizardState currentStep={1} totalSteps={4} completedSteps={[]} skippedSteps={[]} />,
};

export const WizardFlowInProgress = {
  name: 'Wizard / In Progress',
  render: () => <WizardState currentStep={3} totalSteps={4} completedSteps={[1, 2]} skippedSteps={[2]} />,
};

const WizardStepState = ({ activeStep, displayStep }: { activeStep: number; displayStep: number }) => {
  const goToStep = useWizardStore((s) => s.goToStep);

  useEffect(() => {
    goToStep(activeStep);
  }, [activeStep, goToStep]);

  return (
    <WizardStep stepNumber={displayStep}>
      <div className="rounded-md border p-4">
        <h3 className="font-semibold">Step {displayStep} Content</h3>
        <p className="text-sm text-muted-foreground">This content is visible because the current step matches.</p>
      </div>
    </WizardStep>
  );
};

export const WizardStepVisible = {
  name: 'WizardStep / Visible (step matches)',
  render: () => <WizardStepState activeStep={2} displayStep={2} />,
};

export const WizardStepHidden = {
  name: 'WizardStep / Hidden (step mismatch)',
  render: () => (
    <div>
      <WizardStepState activeStep={1} displayStep={3} />
      <p className="text-sm text-muted-foreground">Step 3 content is hidden because current step is 1.</p>
    </div>
  ),
};
