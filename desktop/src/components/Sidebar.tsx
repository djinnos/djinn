import { useSidebarStore } from '@/stores/sidebarStore';
import { useAuthStore } from '@/stores/authStore';
import { Button } from '@/components/ui/button';
import { Kbd } from '@/components/ui/kbd';
import { cn } from '@/lib/utils';

import {
  KanbanIcon,
  Robot01Icon,
  ChatIcon,
  Folder02Icon,
  PlusSignIcon,
  LogoutSquare01Icon,
  PlayIcon,
  PauseIcon,
  Loading02Icon,
  Settings01Icon,
  WorkflowSquare06Icon,
  Brain01Icon,
  Pulse01Icon,
} from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import logoSvg from '@/assets/logo.svg';
import { useEffect, useCallback, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { useLocation, useNavigate } from 'react-router-dom';
import { useExecutionStatus } from '@/hooks/useExecutionStatus';
import { useExecutionControl } from '@/hooks/useExecutionControl';
import { useProjects, useSelectedProjectId } from '@/stores/useProjectStore';
import { ALL_PROJECTS, projectStore } from '@/stores/projectStore';
import { useProjectRoute } from '@/hooks/useProjectRoute';
import { useStore } from 'zustand';
import { verificationStore, type VerificationRun } from '@/stores/verificationStore';
import { addProject, fetchProjects } from '@/api/server';
import { showToast } from '@/lib/toast';
import { HealthCheckPanel } from '@/components/HealthCheckPanel';
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from '@/components/ui/alert-dialog';
import {
  markProposalDraftNotified,
  pulseProposalListQueryOptions,
  shouldNotifyForProposalDraft,
} from '@/lib/pulseProposals';

interface NavItemProps {
  icon: React.ReactNode;
  label: string;
  hotkey?: string;
  badgeCount?: number;
  isActive: boolean;
  onClick: () => void;
}

function NavItem({ icon, label, hotkey, badgeCount, isActive, onClick }: NavItemProps) {
  const pendingProposalLabel =
    typeof badgeCount === 'number' && badgeCount > 0
      ? `${label} has ${badgeCount} pending proposals`
      : undefined;

  return (
    <Button
      variant={isActive ? 'secondary' : 'ghost'}
      size="default"
      onClick={onClick}
      aria-label={pendingProposalLabel}
      className={cn(
        'w-full justify-start gap-3 transition-all duration-200',
        'h-9 px-3',
        isActive && 'bg-white/[0.05] text-foreground'
      )}
    >
      <span className="flex h-4 w-4 items-center justify-center shrink-0">
        {icon}
      </span>
      <span className="text-sm font-medium truncate flex-1 text-left">{label}</span>
      {typeof badgeCount === 'number' && badgeCount > 0 ? (
        <span className="inline-flex min-w-5 items-center justify-center rounded-full bg-primary px-1.5 py-0.5 text-[11px] font-semibold leading-none text-primary-foreground">
          {badgeCount}
        </span>
      ) : null}
      {hotkey && (
        <Kbd>{hotkey.toUpperCase()}</Kbd>
      )}
    </Button>
  );
}

function StatusDot({ state, healthState, tooltip, onClick }: { state: "running" | "paused" | "idle"; healthState?: 'checking' | 'healthy' | 'unhealthy'; tooltip?: string; onClick?: () => void; }) {
  return (
    <span className="relative flex h-2.5 w-2.5 shrink-0">
      {(healthState === 'healthy' && state === "running") && (
        <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-emerald-400 opacity-75" />
      )}
      {healthState === 'checking' && (
        <span className="absolute inline-flex h-full w-full animate-pulse rounded-full bg-yellow-400 opacity-75" />
      )}
      <button
        type="button"
        onClick={(e) => { e.stopPropagation(); onClick?.(); }}
        className={cn("relative inline-flex h-2.5 w-2.5 rounded-full", onClick && "cursor-pointer",
          healthState === 'unhealthy' && "bg-red-500",
          healthState === 'checking' && "bg-yellow-400",
          healthState === 'healthy' && state === "running" && "bg-emerald-400",
          !healthState && state === "running" && "bg-emerald-400",
          state === "paused" && !healthState && "opacity-0",
          !healthState && state === "idle" && "bg-zinc-500",
          state === "idle" && !healthState && "opacity-0"
        )}
        title={tooltip}
        aria-label={tooltip || 'Project status'}
      />
    </span>
  );
}

type ProjectExecState = "active" | "paused" | "idle";

function ProjectExecToggle({
  label,
  projectPath,
  execState,
  isSelected = false,
  onToggle,
}: {
  label: string;
  /** null = all projects scope */
  projectPath: string | null;
  execState: ProjectExecState;
  isSelected?: boolean;
  onToggle: (projectPath: string | null, action: "start" | "pause" | "resume") => Promise<void>;
}) {
  const [open, setOpen] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const isRunning = execState === "active";
  const isPaused = execState === "paused";
  const actionLabel = isRunning ? "Pause" : isPaused ? "Resume" : "Start";
  const action = isRunning ? "pause" as const : isPaused ? "resume" as const : "start" as const;
  const progressLabel = isRunning ? "Pausing..." : isPaused ? "Resuming..." : "Starting...";

  const handleConfirm = async () => {
    setConfirming(true);
    try {
      await onToggle(projectPath, action);
    } finally {
      setConfirming(false);
      setOpen(false);
    }
  };

  return (
    <AlertDialog open={open} onOpenChange={(v) => { if (!confirming) setOpen(v); }}>
      <AlertDialogTrigger
        render={
          <button
            type="button"
            className={cn(
              'flex h-5 w-5 items-center justify-center rounded transition-all',
              'hover:bg-white/10',
              isSelected || isRunning || confirming ? 'opacity-100' : 'opacity-0 group-hover/project:opacity-100'
            )}
            title={`${actionLabel} ${label}`}
            onClick={(e) => e.stopPropagation()}
          />
        }
      >
        {confirming ? (
          <HugeiconsIcon icon={Loading02Icon} size={12} className="animate-spin text-muted-foreground" />
        ) : isRunning ? (
          <HugeiconsIcon icon={PauseIcon} size={12} className="text-red-400" />
        ) : (
          <HugeiconsIcon icon={PlayIcon} size={12} className="text-emerald-400" />
        )}
      </AlertDialogTrigger>
      <AlertDialogContent size="sm">
        <AlertDialogHeader>
          <AlertDialogTitle>{actionLabel} {label}?</AlertDialogTitle>
          <AlertDialogDescription>
            {isRunning
              ? `This will pause all running sessions for ${label}.`
              : isPaused
                ? `This will resume execution for ${label}.`
                : `This will start the execution engine for ${label}.`}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel disabled={confirming}>Cancel</AlertDialogCancel>
          <Button
            variant={isRunning ? "destructive" : "default"}
            disabled={confirming}
            onClick={() => void handleConfirm()}
          >
            {confirming ? (
              <>
                <HugeiconsIcon icon={Loading02Icon} size={16} className="animate-spin" />
                {progressLabel}
              </>
            ) : (
              actionLabel
            )}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

function ProjectListItem({
  name,
  icon,
  isSelected,
  execState,
  healthRun,
  onClick,
  toggleSlot,
  hotkey,
}: {
  name: string;
  icon?: React.ReactNode;
  isSelected: boolean;
  execState?: ProjectExecState;
  healthRun: VerificationRun | null;
  onClick: () => void;
  toggleSlot?: React.ReactNode;
  hotkey?: string;
}) {
  const isActive = execState === "active";
  const [healthPanelOpen, setHealthPanelOpen] = useState(false);

  const failedStep = healthRun?.steps.find((step) => step.status === 'failed');
  const runningStep = healthRun?.steps.find((step) => step.status === 'running');
  const healthState: 'checking' | 'healthy' | 'unhealthy' | undefined =
    healthRun?.status === 'running'
      ? 'checking'
      : healthRun?.status === 'failed'
        ? 'unhealthy'
        : healthRun?.status === 'passed' || healthRun?.status === 'cache_hit'
          ? 'healthy'
          : undefined;

  const tooltip =
    healthState === 'checking'
      ? `Running health check...${runningStep?.name ? ` ${runningStep.name}` : ''}`
      : healthState === 'unhealthy'
        ? (failedStep?.stderr?.split('\n')[0] || failedStep?.stdout?.split('\n')[0] || failedStep?.name || 'Health check failed')
        : healthState === 'healthy' && isActive
          ? 'Healthy — running'
          : execState === 'paused'
            ? 'Paused'
            : undefined;

  const dotState: 'running' | 'paused' | 'idle' = execState === 'paused' ? 'paused' : isActive ? 'running' : 'idle';
  const canOpenPanel = healthState === 'checking' || healthState === 'unhealthy';

  return (
    <>
      <div className="group/project relative flex items-center">
        <div
          role="button"
          tabIndex={0}
          onClick={onClick}
          onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onClick(); } }}
          className={cn(
            'flex w-full items-center gap-2.5 rounded-md px-3 py-1.5 text-sm transition-colors cursor-pointer',
            isSelected
              ? 'bg-white/[0.07] text-foreground font-medium'
              : 'text-muted-foreground hover:bg-white/[0.04] hover:text-foreground'
          )}
        >
          {(isActive || healthState) ? (
            <StatusDot
              state={dotState}
              healthState={healthState}
              tooltip={tooltip}
              onClick={canOpenPanel ? () => setHealthPanelOpen(true) : undefined}
            />
          ) : (
            icon ?? <HugeiconsIcon icon={Folder02Icon} className="h-3.5 w-3.5 shrink-0" />
          )}
          <span className="truncate flex-1 text-left">{name}</span>
          {toggleSlot && (
            <span className="shrink-0">{toggleSlot}</span>
          )}
          {hotkey && (
            <Kbd className="shrink-0">{hotkey}</Kbd>
          )}
        </div>
      </div>
      <HealthCheckPanel
        projectName={name}
        run={healthRun}
        open={healthPanelOpen}
        onClose={() => setHealthPanelOpen(false)}
      />
    </>
  );
}

/** Self-contained project row that polls its own execution status. */
function ProjectRow({
  projectPath,
  label,
  icon,
  isSelected,
  onClick,
  hotkey,
}: {
  projectPath: string | null;
  label: string;
  icon?: React.ReactNode;
  isSelected: boolean;
  onClick: () => void;
  hotkey?: string;
}) {
  const { state, refresh } = useExecutionStatus(projectPath);
  const { start, pause, resume } = useExecutionControl(refresh);
  const healthRun = useStore(verificationStore, useCallback((storeState) => {
    if (!projectPath) return null;

    let latest: VerificationRun | null = null;
    for (const run of storeState.runs.values()) {
      if (run.projectId !== projectPath) continue;
      if (!latest || new Date(run.startedAt).getTime() > new Date(latest.startedAt).getTime()) {
        latest = run;
      }
    }
    return latest;
  }, [projectPath]));

  const execState: ProjectExecState = state === "active" ? "active" : state === "paused" ? "paused" : "idle";

  const handleToggle = useCallback(
    async (_path: string | null, action: "start" | "pause" | "resume") => {
      if (action === "start") await start(projectPath);
      else if (action === "pause") await pause(projectPath);
      else await resume(projectPath);
    },
    [start, pause, resume, projectPath]
  );

  return (
    <ProjectListItem
      name={label}
      icon={icon}
      isSelected={isSelected}
      execState={execState}
      healthRun={healthRun}
      onClick={onClick}
      hotkey={hotkey}
      toggleSlot={
        <ProjectExecToggle
          label={label}
          projectPath={projectPath}
          execState={execState}
          isSelected={isSelected}
          onToggle={handleToggle}
        />
      }
    />
  );
}

function UserFooter() {
  const { user, logout } = useAuthStore();

  if (!user) return null;

  return (
    <div className="flex items-center gap-2.5 rounded-md px-2 py-2">
      {user.picture ? (
        <img src={user.picture} alt="" className="h-7 w-7 shrink-0 rounded-full" />
      ) : (
        <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full bg-muted text-xs font-medium">
          {(user.name?.[0] || user.email?.[0] || '?').toUpperCase()}
        </div>
      )}
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm font-medium text-sidebar-foreground">{user.name || 'User'}</p>
        {user.email && (
          <p className="truncate text-[11px] text-muted-foreground">{user.email}</p>
        )}
      </div>
      <button
        type="button"
        onClick={() => void logout()}
        className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-white/10 hover:text-foreground"
        title="Sign out"
      >
        <HugeiconsIcon icon={LogoutSquare01Icon} className="h-3.5 w-3.5" />
      </button>
    </div>
  );
}

export function Sidebar() {
  const { activeSection, setActiveSection } = useSidebarStore();
  const [isAddingProject, setIsAddingProject] = useState(false);
  const navigate = useNavigate();
  const location = useLocation();
  const projects = useProjects();
  const selectedProjectId = useSelectedProjectId();
  const isAll = selectedProjectId === ALL_PROJECTS;
  const { navigateToProject, navigateToView } = useProjectRoute();
  const user = useAuthStore((state) => state.user);
  const selectedProjectPath = projects.find((project) => project.id === selectedProjectId)?.path ?? '';
  const pulseProposalsQuery = useQuery({
    ...pulseProposalListQueryOptions(selectedProjectPath),
    enabled: !!selectedProjectPath,
  });
  const pulseProposalCount = selectedProjectPath ? (pulseProposalsQuery.data?.length ?? 0) : 0;

  // Sync active section from URL
  useEffect(() => {
    if (location.pathname.includes('/chat')) {
      setActiveSection('chat');
    } else if (location.pathname.includes('/roadmap')) {
      setActiveSection('roadmap');
    } else if (location.pathname.includes('/agents') || location.pathname.includes('/metrics')) {
      setActiveSection('agents');
    } else if (location.pathname.includes('/memory')) {
      setActiveSection('memory');
    } else if (location.pathname.includes('/pulse')) {
      setActiveSection('pulse');
    } else if (location.pathname.startsWith('/settings')) {
      setActiveSection('settings');
    } else {
      setActiveSection('kanban');
    }
  }, [location.pathname, setActiveSection]);

  const handleKeyDown = useCallback((e: KeyboardEvent) => {
    const tag = (e.target as HTMLElement).tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || (e.target as HTMLElement).isContentEditable) return;
    if (e.metaKey || e.ctrlKey || e.altKey) return;

    // Number keys: 0 = All Projects, 1-9 = project by position
    if (e.key >= '0' && e.key <= '9') {
      e.preventDefault();
      const idx = Number(e.key);
      if (idx === 0) {
        navigateToProject(ALL_PROJECTS);
      } else if (idx <= projects.length) {
        navigateToProject(projects[idx - 1].id);
      }
      return;
    }

    switch (e.key.toLowerCase()) {
      case 'c':
        e.preventDefault();
        navigateToView('chat');
        break;
      case 'k':
        e.preventDefault();
        navigateToView('kanban');
        break;
      case 'r':
        e.preventDefault();
        navigateToView('roadmap');
        break;
      case 'a':
        e.preventDefault();
        navigateToView('agents');
        break;
      case 'm':
        e.preventDefault();
        navigateToView('memory');
        break;
      case 'p':
        e.preventDefault();
        navigateToView('pulse');
        break;
      case 's':
        e.preventDefault();
        navigate('/settings');
        break;
    }
  }, [navigate, navigateToView, navigateToProject, projects]);

  useEffect(() => {
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [handleKeyDown]);

  useEffect(() => {
    for (const proposal of pulseProposalsQuery.data ?? []) {
      if (!shouldNotifyForProposalDraft(proposal, user)) continue;

      markProposalDraftNotified(proposal.id);
      showToast.info('Architect proposal draft is ready', {
        description: proposal.originating_spike_id
          ? `Spike ${proposal.originating_spike_id} produced "${proposal.title || proposal.id}".`
          : `"${proposal.title || proposal.id}" is ready for review in Pulse.`,
      });
    }
  }, [pulseProposalsQuery.data, user]);

  const handleAddProject = useCallback(async () => {
    setIsAddingProject(true);
    try {
      // Project selection is being replaced with GitHub-repo selection
      // (Migration 2). In the interim, accept a project path via prompt
      // so local dev against the Docker-mounted workspace still works.
      const path = window.prompt(
        'Project path (relative to the server workspace root, e.g. /workspace/my-project):'
      );
      if (!path) return;
      await addProject(path);
      const projects = await fetchProjects();
      projectStore.getState().setProjects(projects);
      showToast.success('Project added');
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to add project';
      showToast.error('Could not add project', { description: message });
    } finally {
      setIsAddingProject(false);
    }
  }, []);

  return (
    <aside className="flex h-screen w-64 shrink-0 flex-col border-r bg-sidebar">
      {/* Header */}
      <div data-drag-region className="flex h-12 items-center border-b px-5">
        <div className="flex flex-1 items-center gap-3">
          <span className="flex h-4 w-4 items-center justify-center shrink-0 overflow-visible">
            <img src={logoSvg} alt="Djinn" className="h-6 w-6" />
          </span>
          <span className="text-sm font-semibold text-sidebar-foreground truncate">
            Djinn
          </span>
        </div>
      </div>

      {/* Navigation */}
      <nav className="flex-1 p-2 space-y-1 overflow-y-auto">
        <NavItem
          icon={<HugeiconsIcon icon={ChatIcon} className="h-4 w-4" />}
          label="Chat"
          hotkey="C"
          isActive={activeSection === 'chat'}
          onClick={() => navigateToView('chat')}
        />
        <NavItem
          icon={<HugeiconsIcon icon={KanbanIcon} className="h-4 w-4" />}
          label="Kanban"
          hotkey="K"
          isActive={activeSection === 'kanban'}
          onClick={() => navigateToView('kanban')}
        />
        <NavItem
          icon={<HugeiconsIcon icon={WorkflowSquare06Icon} className="h-4 w-4" />}
          label="Roadmap"
          hotkey="R"
          isActive={activeSection === 'roadmap'}
          onClick={() => navigateToView('roadmap')}
        />
        <NavItem
          icon={<HugeiconsIcon icon={Pulse01Icon} className="h-4 w-4" />}
          label="Pulse"
          hotkey="P"
          badgeCount={pulseProposalCount}
          isActive={activeSection === 'pulse'}
          onClick={() => navigateToView('pulse')}
        />
        <NavItem
          icon={<HugeiconsIcon icon={Robot01Icon} className="h-4 w-4" />}
          label="Agents"
          hotkey="A"
          isActive={activeSection === 'agents'}
          onClick={() => navigateToView('agents')}
        />
        <NavItem
          icon={<HugeiconsIcon icon={Brain01Icon} className="h-4 w-4" />}
          label="Memory"
          hotkey="M"
          isActive={activeSection === 'memory'}
          onClick={() => navigateToView('memory')}
        />


        {/* Projects Section */}
        <div className="pt-2 space-y-0.5">
          {/* All Projects row */}
          <ProjectRow
            projectPath={null}
            label="All Projects"
            icon={<HugeiconsIcon icon={Folder02Icon} className="h-3.5 w-3.5" />}
            isSelected={isAll}
            onClick={() => navigateToProject(ALL_PROJECTS)}
            hotkey="0"
          />

          {/* Individual project rows */}
          {projects.map((project, idx) => (
            <ProjectRow
              key={project.id}
              projectPath={project.path ?? null}
              label={project.name}
              icon={<HugeiconsIcon icon={Folder02Icon} className="h-3.5 w-3.5" />}
              isSelected={selectedProjectId === project.id}
              onClick={() => navigateToProject(project.id)}
              hotkey={idx < 9 ? String(idx + 1) : undefined}
            />
          ))}

          {/* Add Project */}
          <div
            role="button"
            tabIndex={0}
            onClick={() => !isAddingProject && void handleAddProject()}
            onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); if (!isAddingProject) void handleAddProject(); } }}
            className={cn(
              'flex w-full items-center gap-2.5 rounded-md px-3 py-1.5 text-sm transition-colors cursor-pointer',
              'text-muted-foreground hover:bg-white/[0.04] hover:text-foreground',
              isAddingProject && 'opacity-50 cursor-not-allowed'
            )}
          >
            {isAddingProject ? (
              <HugeiconsIcon icon={Loading02Icon} size={14} className="shrink-0 animate-spin" />
            ) : (
              <HugeiconsIcon icon={PlusSignIcon} className="h-3.5 w-3.5 shrink-0" />
            )}
            <span className="truncate flex-1 text-left">Add Project</span>
          </div>
        </div>
      </nav>

      {/* Footer */}
      <div className="border-t p-3 space-y-2">
        <NavItem
          icon={<HugeiconsIcon icon={Settings01Icon} size={16} />}
          label="Settings"
          hotkey="S"
          isActive={activeSection === 'settings'}
          onClick={() => navigate('/settings')}
        />
        <UserFooter />
      </div>
    </aside>
  );
}
