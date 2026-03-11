import { useSidebarStore } from '@/stores/sidebarStore';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import {
  Settings,
  PanelLeft,
  Command,
  Play,
  Pause,
  Loader2,
  ChevronDown,
  ChevronRight,
  Layers,
  FolderOpen,
  Plus,
} from 'lucide-react';
import { Flag02Icon, KanbanIcon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import logoSvg from '@/assets/logo.svg';
import { useEffect, useCallback, useState } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { useExecutionStatus } from '@/hooks/useExecutionStatus';
import { useExecutionControl } from '@/hooks/useExecutionControl';
import { useProjects, useSelectedProjectId } from '@/stores/useProjectStore';
import { ALL_PROJECTS } from '@/stores/projectStore';
import { useProjectRoute } from '@/hooks/useProjectRoute';
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


interface NavItemProps {
  icon: React.ReactNode;
  label: string;
  hotkey?: string;
  isActive: boolean;
  isCollapsed: boolean;
  onClick: () => void;
}

function NavItem({ icon, label, hotkey, isActive, isCollapsed, onClick }: NavItemProps) {
  return (
    <Button
      variant={isActive ? 'secondary' : 'ghost'}
      size={isCollapsed ? 'icon' : 'default'}
      onClick={onClick}
      className={cn(
        'w-full justify-start gap-3 transition-all duration-200',
        isCollapsed ? 'h-10 w-10 justify-center' : 'h-9 px-3',
        isActive && 'bg-white/[0.05] text-foreground'
      )}
      title={isCollapsed ? `${label}${hotkey ? ` (${hotkey.toUpperCase()})` : ''}` : undefined}
    >
      <span className="flex h-4 w-4 items-center justify-center shrink-0">
        {icon}
      </span>
      {!isCollapsed && (
        <>
          <span className="text-sm font-medium truncate flex-1 text-left">{label}</span>
          {hotkey && (
            <kbd className="inline-flex h-4 items-center justify-center rounded border border-sidebar-border px-1 font-mono text-[10px] text-muted-foreground/50">
              {hotkey.toUpperCase()}
            </kbd>
          )}
        </>
      )}
    </Button>
  );
}

function StatusDot({ state, pulsing = false }: { state: "running" | "paused" | "idle"; pulsing?: boolean }) {
  return (
    <span className="relative flex h-2.5 w-2.5 shrink-0">
      {state === "running" && pulsing && (
        <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-emerald-400 opacity-75" />
      )}
      <span
        className={cn(
          "relative inline-flex h-2.5 w-2.5 rounded-full",
          state === "running" && "bg-emerald-400",
          state === "paused" && "bg-yellow-400",
          state === "idle" && "bg-zinc-500"
        )}
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
          <Loader2 className="h-3 w-3 animate-spin text-muted-foreground" />
        ) : isRunning ? (
          <Pause className="h-3 w-3 text-red-400" />
        ) : (
          <Play className="h-3 w-3 text-muted-foreground" />
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
                <Loader2 className="h-4 w-4 animate-spin" />
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
  isCollapsed,
  execState,
  onClick,
  toggleSlot,
}: {
  name: string;
  icon?: React.ReactNode;
  isSelected: boolean;
  isCollapsed: boolean;
  execState?: ProjectExecState;
  onClick: () => void;
  toggleSlot?: React.ReactNode;
}) {
  const isActive = execState === "active";

  return (
    <div className="group/project relative flex items-center">
      <button
        type="button"
        onClick={onClick}
        className={cn(
          'flex w-full items-center gap-2.5 rounded-md px-3 py-1.5 text-sm transition-colors',
          isCollapsed ? 'justify-center px-0' : '',
          isSelected
            ? 'bg-white/[0.07] text-foreground font-medium'
            : 'text-muted-foreground hover:bg-white/[0.04] hover:text-foreground'
        )}
        title={isCollapsed ? name : undefined}
      >
        {isActive ? (
          <StatusDot state="running" pulsing />
        ) : (
          icon ?? <FolderOpen className="h-3.5 w-3.5 shrink-0" />
        )}
        {!isCollapsed && (
          <>
            <span className="truncate flex-1 text-left">{name}</span>
            {toggleSlot && (
              <span className="shrink-0">{toggleSlot}</span>
            )}
          </>
        )}
      </button>
    </div>
  );
}

/** Self-contained project row that polls its own execution status. */
function ProjectRow({
  projectPath,
  label,
  icon,
  isSelected,
  isCollapsed,
  onClick,
}: {
  projectPath: string | null;
  label: string;
  icon?: React.ReactNode;
  isSelected: boolean;
  isCollapsed: boolean;
  onClick: () => void;
}) {
  const { state, refresh } = useExecutionStatus(projectPath);
  const { start, pause, resume } = useExecutionControl(refresh);

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
      isCollapsed={isCollapsed}
      execState={execState}
      onClick={onClick}
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

export function Sidebar() {
  const { isCollapsed, activeSection, projectsExpanded, toggleCollapse, setActiveSection, setProjectsExpanded } = useSidebarStore();
  const navigate = useNavigate();
  const location = useLocation();
  const projects = useProjects();
  const selectedProjectId = useSelectedProjectId();
  const isAll = selectedProjectId === ALL_PROJECTS;
  const { navigateToProject, navigateToView } = useProjectRoute();

  // Sync active section from URL
  useEffect(() => {
    if (location.pathname.includes('/epics')) {
      setActiveSection('epics');
    } else if (location.pathname.startsWith('/settings')) {
      setActiveSection('settings');
    } else {
      setActiveSection('kanban');
    }
  }, [location.pathname, setActiveSection]);

  const handleKeyDown = useCallback((e: KeyboardEvent) => {
    if ((e.metaKey || e.ctrlKey) && e.key === '/') {
      e.preventDefault();
      toggleCollapse();
      return;
    }

    const tag = (e.target as HTMLElement).tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || (e.target as HTMLElement).isContentEditable) return;
    if (e.metaKey || e.ctrlKey || e.altKey) return;

    switch (e.key.toLowerCase()) {
      case 'k':
        e.preventDefault();
        navigateToView('kanban');
        break;
      case 'e':
        e.preventDefault();
        navigateToView('epics');
        break;
      case 's':
        e.preventDefault();
        navigate('/settings');
        break;
    }
  }, [toggleCollapse, navigate, navigateToView]);

  useEffect(() => {
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [handleKeyDown]);

  return (
    <aside
      className={cn(
        'flex h-screen shrink-0 flex-col border-r bg-sidebar transition-all duration-200 ease-in-out',
        isCollapsed ? 'w-14' : 'w-64'
      )}
    >
      {/* Header */}
      <div data-tauri-drag-region className={cn("flex h-12 items-center border-b", isCollapsed ? "justify-center px-2" : "px-5")}>
        <div className={cn("flex items-center gap-3", !isCollapsed && "flex-1")}>
          <span className="flex h-4 w-4 items-center justify-center shrink-0">
            <img src={logoSvg} alt="Djinn" className="h-4 w-4" />
          </span>
          {!isCollapsed && (
            <span className="text-sm font-semibold text-sidebar-foreground truncate">
              Djinn
            </span>
          )}
        </div>
        {!isCollapsed && (
          <>
            <div className="flex items-center gap-1 text-[10px] text-sidebar-foreground/50">
              <kbd className="inline-flex h-4 items-center justify-center rounded border border-sidebar-border px-1 font-mono">
                <Command className="h-2.5 w-2.5" />
              </kbd>
              <kbd className="inline-flex h-4 items-center justify-center rounded border border-sidebar-border px-1 font-mono">
                /
              </kbd>
            </div>
            <Button
              variant="ghost"
              size="icon"
              onClick={toggleCollapse}
              className="h-8 w-8 shrink-0"
              title="Collapse sidebar (Cmd+/)"
            >
              <PanelLeft className="h-4 w-4 transition-transform duration-200" />
            </Button>
          </>
        )}
      </div>

      {/* Navigation */}
      <nav className="flex-1 overflow-y-auto p-2 space-y-4">
        {/* Projects Section */}
        <div className="space-y-1">
          {!isCollapsed && (
            <button
              type="button"
              onClick={() => setProjectsExpanded(!projectsExpanded)}
              className="flex w-full items-center gap-1.5 px-2 py-1 text-[11px] font-medium uppercase tracking-wider text-muted-foreground/60 hover:text-muted-foreground transition-colors"
            >
              {projectsExpanded ? (
                <ChevronDown className="h-3 w-3" />
              ) : (
                <ChevronRight className="h-3 w-3" />
              )}
              Projects
            </button>
          )}

          {(isCollapsed || projectsExpanded) && (
            <div className="space-y-0.5">
              {/* All Projects */}
              <ProjectRow
                projectPath={null}
                label="All Projects"
                icon={<Layers className="h-3.5 w-3.5 shrink-0" />}
                isSelected={isAll}
                isCollapsed={isCollapsed}
                onClick={() => navigateToProject(ALL_PROJECTS)}
              />

              {/* Individual projects */}
              {projects.map((project) => (
                <ProjectRow
                  key={project.id}
                  projectPath={project.path ?? null}
                  label={project.name}
                  isSelected={!isAll && selectedProjectId === project.id}
                  isCollapsed={isCollapsed}
                  onClick={() => navigateToProject(project.id)}
                />
              ))}

              {/* Add project */}
              {!isCollapsed && (
                <button
                  type="button"
                  onClick={() => navigate('/settings/projects')}
                  className="flex w-full items-center gap-2.5 rounded-md px-3 py-1.5 text-sm text-muted-foreground/50 transition-colors hover:bg-white/[0.04] hover:text-muted-foreground"
                >
                  <Plus className="h-3.5 w-3.5 shrink-0" />
                  <span>New Project</span>
                </button>
              )}
            </div>
          )}
        </div>

        {/* Separator */}
        <div className="mx-2 h-px bg-sidebar-border" />

        {/* Views Section */}
        <div className="space-y-1">
          {!isCollapsed && (
            <div className="px-2 py-1 text-[11px] font-medium uppercase tracking-wider text-muted-foreground/60">
              Views
            </div>
          )}
          <NavItem
            icon={<HugeiconsIcon icon={KanbanIcon} size={16} />}
            label="Kanban"
            hotkey="k"
            isActive={activeSection === 'kanban'}
            isCollapsed={isCollapsed}
            onClick={() => navigateToView('kanban')}
          />
          <NavItem
            icon={<HugeiconsIcon icon={Flag02Icon} size={16} />}
            label="Epics"
            hotkey="e"
            isActive={activeSection === 'epics'}
            isCollapsed={isCollapsed}
            onClick={() => navigateToView('epics')}
          />
        </div>

        {/* Separator */}
        <div className="mx-2 h-px bg-sidebar-border" />

        {/* Settings */}
        <NavItem
          icon={<Settings className="h-4 w-4" />}
          label="Settings"
          hotkey="s"
          isActive={activeSection === 'settings'}
          isCollapsed={isCollapsed}
          onClick={() => navigate('/settings')}
        />
      </nav>
    </aside>
  );
}
