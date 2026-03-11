import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  Cancel01Icon,
  MinusSignIcon,
  SquareIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { ChevronRight, Layers } from "lucide-react";
import { useSelectedProject, useIsAllProjects } from "@/stores/useProjectStore";
import { useProjectRoute } from "@/hooks/useProjectRoute";
import { useState, useRef, useEffect } from "react";
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

const VIEW_LABELS: Record<string, string> = {
  kanban: "Kanban",
  epics: "Epics",
};

function Breadcrumb() {
  const selected = useSelectedProject();
  const isAll = useIsAllProjects();
  const { currentView } = useProjectRoute();

  const projectLabel = isAll ? "All Projects" : selected?.name ?? "No project";
  const viewLabel = currentView ? VIEW_LABELS[currentView] : null;

  return (
    <div className="flex items-center gap-1.5 text-xs font-medium text-foreground/80">
      {isAll && <Layers className="h-3 w-3 shrink-0 text-muted-foreground" />}
      <span className="max-w-[160px] truncate">{projectLabel}</span>
      {viewLabel && (
        <>
          <ChevronRight className="h-3 w-3 shrink-0 text-muted-foreground/50" />
          <span className="text-muted-foreground">{viewLabel}</span>
        </>
      )}
    </div>
  );
}

function formatModelName(modelId: string): string {
  // "openai/gpt-5.3-codex" → "gpt-5.3-codex"
  const parts = modelId.split("/");
  return parts.length > 1 ? parts.slice(1).join("/") : modelId;
}

function ExecutionIndicator() {
  const { state, runningSessions, maxSessions, capacity } = useExecutionStatus();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  // Close on outside click
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  if (state === null) return null;

  const capacityEntries = Object.entries(capacity);

  return (
    <div ref={ref} className="relative">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex items-center gap-1.5 rounded px-2 py-0.5 text-xs font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground outline-none"
      >
        <span className="tabular-nums">{runningSessions}/{maxSessions}</span>
        <span>sessions</span>
      </button>
      {open && (
        <div className="absolute top-full left-1/2 z-50 mt-1 min-w-[180px] -translate-x-1/2 rounded-lg bg-popover p-1 text-popover-foreground shadow-md ring-1 ring-foreground/10 animate-in fade-in-0 zoom-in-95">
          <div className="px-2 py-1 text-xs font-medium text-muted-foreground">
            Capacity
          </div>
          <div className="-mx-1 my-1 h-px bg-border" />
          {capacityEntries.length === 0 ? (
            <div className="px-2 py-1.5 text-xs text-muted-foreground">
              No models configured
            </div>
          ) : (
            capacityEntries.map(([modelId, cap]) => (
              <div
                key={modelId}
                className="flex items-center justify-between gap-4 px-2 py-1.5 text-xs"
              >
                <span className="truncate text-muted-foreground">
                  {formatModelName(modelId)}
                </span>
                <span className="shrink-0 tabular-nums font-medium">
                  {cap.active}/{cap.max}
                </span>
              </div>
            ))
          )}
        </div>
      )}
    </div>
  );
}

export function Titlebar() {
  return (
    <div
      data-tauri-drag-region
      className="flex h-9 select-none items-center border-b border-border/50 bg-background"
    >
      {/* Left: Breadcrumb */}
      <div className="flex items-center pl-3">
        <Breadcrumb />
      </div>

      {/* Center: Session indicator */}
      <div data-tauri-drag-region className="flex flex-1 items-center justify-center">
        <ExecutionIndicator />
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
