import { useSidebarStore } from '@/stores/sidebarStore';
import { Button, buttonVariants } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import {
  Settings,
  PanelLeft,
  Command,
  Play,
  Pause,
  Loader2,
  ChevronDown,
  Layers,
} from 'lucide-react';
import { Flag02Icon, KanbanIcon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import logoSvg from '@/assets/logo.svg';
import { useEffect, useCallback, useState } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { useExecutionStatus } from '@/hooks/useExecutionStatus';
import { useExecutionControl } from '@/hooks/useExecutionControl';
import { useSelectedProject, useIsAllProjects, useProjects } from '@/stores/useProjectStore';
import { taskStore } from '@/stores/taskStore';
import { projectStore } from '@/stores/projectStore';
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
} from '@/components/ui/dropdown-menu';

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

/** Shorten a full model ID like "anthropic/Claude Opus 4.6" → "opus" */
function shortModelName(modelId: string): string {
  const lower = modelId.toLowerCase();
  if (lower.includes("opus")) return "opus";
  if (lower.includes("sonnet")) return "sonnet";
  if (lower.includes("haiku")) return "haiku";
  if (lower.includes("glm")) return "glm";
  // Fallback: take the last segment after / and truncate
  const parts = modelId.split("/");
  return parts[parts.length - 1].slice(0, 12);
}

/** Resolve a task_id (UUID or short_id) to a human-readable title */
function resolveTaskTitle(taskId: string): string {
  const tasks = taskStore.getState().getAllTasks();
  const match = tasks.find((t) => t.id === taskId || t.short_id === taskId);
  return match?.title ?? taskId;
}

/** Resolve a project_id to its name */
function resolveProjectName(projectId: string | undefined): string | undefined {
  if (!projectId) return undefined;
  const projects = projectStore.getState().projects;
  return projects.find((p) => p.id === projectId)?.name;
}

function formatSessionDuration(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  const m = Math.floor(seconds / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

function ExecutionDiagnostics({ scopePath }: { scopePath: string | null }) {
  const { state, runningSessions, maxSessions, raw } = useExecutionStatus(scopePath);
  const [expandedModel, setExpandedModel] = useState<string | null>(null);
  const isGlobal = scopePath === null;

  if (!state) return null;

  const capacity = raw?.capacity as Record<string, { active: number; max: number }> | undefined;
  const metrics = raw?.metrics as { tasks_dispatched: number; sessions_recovered: number } | undefined;
  const sessions = raw?.sessions as Array<{ task_id: string; model_id: string; duration_seconds: number; project_id?: string }> | undefined;
  const error = raw?.error as string | undefined;

  const sessionsForModel = (modelId: string) =>
    sessions?.filter((s) => s.model_id === modelId) ?? [];

  return (
    <div className="mx-2 mb-1 rounded border border-zinc-700 bg-zinc-900/80 px-2 py-1.5 text-[10px] font-mono text-zinc-400 space-y-1">
      <div className="flex justify-between">
        <span>state</span>
        <span className={state === "active" ? "text-emerald-400" : state === "paused" ? "text-yellow-400" : "text-zinc-500"}>
          {state ?? "null"}
        </span>
      </div>
      <div className="flex justify-between">
        <span>sessions</span>
        <span>{runningSessions} / {maxSessions}</span>
      </div>
      {metrics && (
        <div className="flex justify-between">
          <span>dispatched</span>
          <span>{metrics.tasks_dispatched}</span>
        </div>
      )}
      {metrics && metrics.sessions_recovered > 0 && (
        <div className="flex justify-between">
          <span>recovered</span>
          <span>{metrics.sessions_recovered}</span>
        </div>
      )}
      {error && (
        <div className="text-red-400 break-all">err: {error}</div>
      )}
      {capacity && Object.keys(capacity).length > 0 ? (
        <div className="border-t border-zinc-700 pt-1 space-y-0.5">
          <span className="text-zinc-500">capacity:</span>
          {Object.entries(capacity).map(([model, cap]) => {
            const isExpanded = expandedModel === model;
            const modelSessions = sessionsForModel(model);
            const hasActiveSessions = cap.active > 0 && modelSessions.length > 0;
            return (
              <div key={model}>
                <div
                  className={cn(
                    "flex justify-between pl-1 rounded",
                    hasActiveSessions && "cursor-pointer hover:bg-zinc-800"
                  )}
                  onClick={() => hasActiveSessions && setExpandedModel(isExpanded ? null : model)}
                >
                  <span className="truncate max-w-[120px]" title={model}>{shortModelName(model)}</span>
                  <span className={hasActiveSessions ? "text-emerald-400" : undefined}>{cap.active}/{cap.max}</span>
                </div>
                {isExpanded && modelSessions.length > 0 && (
                  <div className="pl-3 space-y-0.5 py-0.5 border-l border-zinc-700 ml-1">
                    {modelSessions.map((s) => {
                      const title = resolveTaskTitle(s.task_id);
                      const projectName = isGlobal ? resolveProjectName(s.project_id) : undefined;
                      return (
                        <div key={s.task_id} className="flex flex-col gap-0.5 py-0.5">
                          <span className="truncate text-zinc-300" title={title}>{title}</span>
                          <div className="flex items-center justify-between gap-1 text-zinc-500">
                            {projectName && <span className="truncate max-w-[80px]">{projectName}</span>}
                            <span className="shrink-0 ml-auto">{formatSessionDuration(s.duration_seconds)}</span>
                          </div>
                        </div>
                      );
                    })}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      ) : (
        <div className="text-yellow-500/80">no model capacity reported</div>
      )}
    </div>
  );
}

/** Scope for execution controls — independent of the project selector view. */
type ExecScope = { type: "all" } | { type: "project"; path: string; name: string };

function ExecutionPanel() {
  const viewProject = useSelectedProject();
  const isAllView = useIsAllProjects();
  const projects = useProjects();

  // Execution scope defaults to follow the project selector view,
  // but the user can override it via the chevron menu.
  const [scopeOverride, setScopeOverride] = useState<ExecScope | null>(null);

  const scope: ExecScope = scopeOverride
    ?? (isAllView
      ? { type: "all" }
      : viewProject?.path
        ? { type: "project", path: viewProject.path, name: viewProject.name }
        : { type: "all" });

  const scopePath = scope.type === "all" ? null : scope.path;
  const { state, runningSessions, refresh } = useExecutionStatus(scopePath);
  const { busy, start, pause, resume } = useExecutionControl(refresh);

  // Server states: "active" (engine on), "paused" (graceful pause), null (not started/loading)
  const isActive = state === "active";
  const isPaused = state === "paused";
  const hasRunningSessions = runningSessions > 0;
  const isIdle = !isActive && !isPaused;
  const dotState = isPaused ? "paused" as const : isActive ? "running" as const : "idle" as const;

  const handleClick = async () => {
    if (busy) return;
    if (isActive) {
      await pause(scopePath);
    } else if (isPaused) {
      await resume(scopePath);
    } else {
      await start(scopePath);
    }
  };

  const scopeLabel = scope.type === "all" ? "All" : scope.name;
  const actionLabel = isActive ? "Pause" : isPaused ? "Resume" : "Start";
  const label = `${actionLabel} ${scopeLabel}`;

  const icon = busy ? (
    <Loader2 className="h-4 w-4 animate-spin" />
  ) : isActive ? (
    <Pause className="h-4 w-4" />
  ) : (
    <Play className="h-4 w-4" />
  );

  const buttonStyles = cn(
    "h-10 transition-all duration-200",
    isActive && "border-emerald-500/40 bg-emerald-500/15 text-emerald-400 hover:bg-emerald-500/25",
    isPaused && "border-yellow-500/40 bg-yellow-500/15 text-yellow-400 hover:bg-yellow-500/25",
    isIdle && "bg-primary/80 text-primary-foreground hover:bg-primary"
  );

  return (
    <>
      <div className="p-2">
        <div className="flex gap-px">
          {/* Main action */}
          <Button
            variant={isActive ? "secondary" : isPaused ? "outline" : "default"}
            size="default"
            onClick={handleClick}
            disabled={busy}
            className={cn(buttonStyles, "flex-1 gap-2 px-3 rounded-r-none")}
          >
            <StatusDot state={dotState} pulsing={hasRunningSessions} />
            {icon}
            <span className="text-sm font-medium truncate flex-1 text-left">{label}</span>
            {hasRunningSessions && (
              <span className="inline-flex h-5 min-w-5 items-center justify-center rounded-full bg-emerald-500/30 px-1.5 text-[10px] font-bold text-emerald-300">
                {runningSessions}
              </span>
            )}
          </Button>

          {/* Scope picker */}
          <DropdownMenu>
            <DropdownMenuTrigger
              className={cn(
                buttonVariants({ variant: isActive ? "secondary" : isPaused ? "outline" : "default", size: "icon" }),
                buttonStyles,
                "w-8 shrink-0 rounded-l-none border-l border-white/10"
              )}
            >
              <ChevronDown className="h-3.5 w-3.5" />
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="min-w-[160px]">
              <DropdownMenuItem
                onClick={() => setScopeOverride({ type: "all" })}
                className={cn(scope.type === "all" && "bg-accent")}
              >
                <Layers className="mr-2 h-3.5 w-3.5 text-muted-foreground" />
                All Projects
              </DropdownMenuItem>
              {projects.length > 0 && <DropdownMenuSeparator />}
              {projects.filter((p) => p.path).map((p) => (
                <DropdownMenuItem
                  key={p.id}
                  onClick={() => setScopeOverride({ type: "project", path: p.path!, name: p.name })}
                  className={cn(scope.type === "project" && scope.path === p.path && "bg-accent")}
                >
                  <span className="truncate">{p.name}</span>
                </DropdownMenuItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </div>
      <ExecutionDiagnostics scopePath={scopePath} />
    </>
  );
}

export function Sidebar() {
  const { isCollapsed, activeSection, toggleCollapse, setActiveSection } = useSidebarStore();
  const navigate = useNavigate();
  const location = useLocation();

  const handleKeyDown = useCallback((e: KeyboardEvent) => {
    if ((e.metaKey || e.ctrlKey) && e.key === '/') {
      e.preventDefault();
      toggleCollapse();
      return;
    }

    // Skip hotkeys when typing in inputs
    const tag = (e.target as HTMLElement).tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || (e.target as HTMLElement).isContentEditable) return;
    if (e.metaKey || e.ctrlKey || e.altKey) return;

    switch (e.key.toLowerCase()) {
      case 'k':
        e.preventDefault();
        navigate('/');
        break;
      case 'e':
        e.preventDefault();
        navigate('/roadmap');
        break;
      case 's':
        e.preventDefault();
        navigate('/settings');
        break;
    }
  }, [toggleCollapse, navigate]);

  useEffect(() => {
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [handleKeyDown]);

  useEffect(() => {
    if (location.pathname.startsWith('/roadmap')) {
      setActiveSection('roadmap');
    } else if (location.pathname.startsWith('/settings')) {
      setActiveSection('settings');
    } else {
      setActiveSection('kanban');
    }
  }, [location.pathname, setActiveSection]);

  const navItems = [
    {
      id: 'kanban' as const,
      label: 'Kanban',
      icon: <HugeiconsIcon icon={KanbanIcon} size={16} />,
      hotkey: 'k',
    },
    {
      id: 'roadmap' as const,
      label: 'Epics',
      icon: <HugeiconsIcon icon={Flag02Icon} size={16} />,
      hotkey: 'e',
    },
  ];

  return (
    <aside
      className={cn(
        'flex h-screen shrink-0 flex-col border-r bg-sidebar transition-all duration-200 ease-in-out',
        isCollapsed ? 'w-14' : 'w-64'
      )}
    >
      {/* Header — px-5 aligns logo with nav icons (nav p-2 + button px-3) */}
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

      {/* Execution Control — hidden when collapsed */}
      {!isCollapsed && <ExecutionPanel />}

      {/* Navigation */}
      <nav className="flex-1 overflow-y-auto p-2 space-y-1">
        {navItems.map((item) => (
          <NavItem
            key={item.id}
            icon={item.icon}
            label={item.label}
            hotkey={item.hotkey}
            isActive={activeSection === item.id}
            isCollapsed={isCollapsed}
            onClick={() => navigate(item.id === 'kanban' ? '/' : `/${item.id}`)}
          />
        ))}
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
