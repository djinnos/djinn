import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  Cancel01Icon,
  MinusSignIcon,
  SquareIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { ChevronDown, Layers } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/utils";
import { useProjects, useSelectedProject, useIsAllProjects } from "@/stores/useProjectStore";
import { projectStore, ALL_PROJECTS } from "@/stores/projectStore";
import { useExecutionStatus } from "@/hooks/useExecutionStatus";

const appWindow = getCurrentWindow();

function TitlebarButton({
  onClick,
  label,
  children,
  variant = "default",
}: {
  onClick: () => void;
  label: string;
  children: React.ReactNode;
  variant?: "default" | "close";
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-label={label}
      className={`flex h-full w-10 items-center justify-center transition-colors ${
        variant === "close"
          ? "hover:bg-red-500/90 hover:text-white"
          : "hover:bg-muted"
      }`}
    >
      {children}
    </button>
  );
}

function ProjectSwitcher() {
  const projects = useProjects();
  const selected = useSelectedProject();
  const isAll = useIsAllProjects();

  const displayName = isAll ? "All Projects" : selected?.name ?? "No project";

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        className="flex items-center gap-1.5 rounded-md px-2 py-1 text-xs font-medium text-foreground/80 transition-colors hover:bg-muted hover:text-foreground"
      >
        {isAll && <Layers className="h-3 w-3 shrink-0 text-muted-foreground" />}
        <span className="max-w-[160px] truncate">{displayName}</span>
        <ChevronDown className="h-3 w-3 shrink-0 text-muted-foreground" />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start">
        <DropdownMenuItem
          onClick={() => projectStore.getState().setSelectedProjectId(ALL_PROJECTS)}
          className={cn(isAll && "bg-accent")}
        >
          <Layers className="mr-2 h-3.5 w-3.5 text-muted-foreground" />
          All Projects
        </DropdownMenuItem>
        {projects.length > 0 && <DropdownMenuSeparator />}
        {projects.map((p) => (
          <DropdownMenuItem
            key={p.id}
            onClick={() => projectStore.getState().setSelectedProjectId(p.id)}
            className={cn(!isAll && selected?.id === p.id && "bg-accent")}
          >
            <span className="truncate">{p.name}</span>
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function ExecutionBadge() {
  const selected = useSelectedProject();
  const isAll = useIsAllProjects();
  const projectPath = isAll ? undefined : selected?.path;
  const { state, runningSessions } = useExecutionStatus(projectPath ?? null);

  if (!state) return null;

  const isActive = state === "active";
  const isPaused = state === "paused";

  return (
    <div className={cn(
      "flex items-center gap-1.5 rounded-full px-2 py-0.5 text-[10px] font-medium",
      isActive && runningSessions > 0 && "bg-emerald-500/15 text-emerald-400",
      isActive && runningSessions === 0 && "bg-emerald-500/10 text-emerald-400/60",
      isPaused && "bg-yellow-500/10 text-yellow-400/60",
      !isActive && !isPaused && "bg-zinc-500/10 text-zinc-400",
    )}>
      <span className={cn(
        "inline-block h-1.5 w-1.5 rounded-full",
        isActive && "bg-emerald-400",
        isPaused && "bg-yellow-400",
        !isActive && !isPaused && "bg-zinc-500",
        isActive && runningSessions > 0 && "animate-pulse",
      )} />
      {isActive
        ? runningSessions > 0 ? `${runningSessions} active` : "idle"
        : isPaused ? "paused" : state}
    </div>
  );
}

export function Titlebar() {
  return (
    <div
      data-tauri-drag-region
      className="flex h-9 select-none items-center border-b border-border/50 bg-background"
    >
      {/* Left: Project switcher */}
      <div className="flex items-center pl-3">
        <ProjectSwitcher />
      </div>

      {/* Center: Execution badge */}
      <div data-tauri-drag-region className="flex flex-1 items-center justify-center">
        <ExecutionBadge />
      </div>

      {/* Right: Window controls */}
      <div className="flex h-full items-center">
        <TitlebarButton onClick={() => appWindow.minimize()} label="Minimize">
          <HugeiconsIcon icon={MinusSignIcon} size={14} className="pointer-events-none" />
        </TitlebarButton>
        <TitlebarButton onClick={() => appWindow.toggleMaximize()} label="Maximize">
          <HugeiconsIcon icon={SquareIcon} size={12} className="pointer-events-none" />
        </TitlebarButton>
        <TitlebarButton onClick={() => appWindow.close()} label="Close" variant="close">
          <HugeiconsIcon icon={Cancel01Icon} size={14} className="pointer-events-none" />
        </TitlebarButton>
      </div>
    </div>
  );
}
