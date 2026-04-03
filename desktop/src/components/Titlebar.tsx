import { getCurrentWindow } from "@/electron/shims/window";
import {
  Cancel01Icon,
  MinusSignIcon,
  SquareIcon,
  ArrowRight01Icon,
  Layers01Icon,
  GitBranchIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { useSelectedProject, useIsAllProjects, projectStore } from "@/stores/useProjectStore";
import { useProjectRoute } from "@/hooks/useProjectRoute";
import { useState, useRef, useEffect, useCallback } from "react";
import { useExecutionStatus } from "@/hooks/useExecutionStatus";
import { updateProject, fetchProjects } from "@/api/server";
import { listGitBranches } from "@/electron/commands";
import {
  Combobox,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxInput,
  ComboboxItem,
  ComboboxList,
} from "@/components/ui/combobox";

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
  chat: "Chat",
};

function Breadcrumb() {
  const selected = useSelectedProject();
  const isAll = useIsAllProjects();
  const { currentView } = useProjectRoute();

  const projectLabel = isAll ? "All Projects" : selected?.name ?? "No project";
  const viewLabel = currentView ? VIEW_LABELS[currentView] : null;

  return (
    <div className="flex items-center gap-1.5 text-xs font-medium text-foreground/80">
      {isAll && <HugeiconsIcon icon={Layers01Icon} size={12} className="shrink-0 text-muted-foreground" />}
      <span className="max-w-[160px] truncate">{projectLabel}</span>
      {viewLabel && (
        <>
          <HugeiconsIcon icon={ArrowRight01Icon} size={12} className="shrink-0 text-muted-foreground/50" />
          <span className="text-muted-foreground">{viewLabel}</span>
        </>
      )}
    </div>
  );
}

function BranchIndicator() {
  const selected = useSelectedProject();
  const isAll = useIsAllProjects();
  const [open, setOpen] = useState(false);
  const [branches, setBranches] = useState<string[]>([]);
  const [inputValue, setInputValue] = useState("");

  const branch = selected?.branch ?? "main";

  // Fetch branches when dropdown opens
  useEffect(() => {
    if (!open || !selected?.path) return;
    listGitBranches(selected.path).then(setBranches).catch(() => setBranches([]));
  }, [open, selected?.path]);

  const selectBranch = useCallback(
    async (val: string | null) => {
      const trimmed = (val ?? inputValue).trim();
      if (!selected || !trimmed || trimmed === branch) {
        setOpen(false);
        return;
      }
      try {
        await updateProject(selected.id, { branch: trimmed });
        const projects = await fetchProjects();
        projectStore.getState().setProjects(projects);
      } catch (err) {
        console.error("Failed to update target branch:", err);
      }
      setOpen(false);
    },
    [selected, branch, inputValue],
  );

  if (isAll || !selected) return null;

  // Check if typed value matches an existing branch
  const trimmedInput = inputValue.trim();
  const isNew = trimmedInput.length > 0 && !branches.some((b) => b === trimmedInput);

  if (!open) {
    return (
      <button
        type="button"
        onClick={() => setOpen(true)}
        className="flex items-center gap-1 rounded px-1.5 py-0.5 text-xs text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
        title="Target branch — click to change"
      >
        <HugeiconsIcon icon={GitBranchIcon} size={12} className="shrink-0" />
        <span className="max-w-[120px] truncate">{branch}</span>
      </button>
    );
  }

  return (
    <Combobox
      open={open}
      onOpenChange={setOpen}
      value={branch}
      onValueChange={selectBranch}
      onInputValueChange={setInputValue}
    >
      <div className="flex items-center gap-1">
        <HugeiconsIcon icon={GitBranchIcon} size={12} className="shrink-0 text-muted-foreground" />
        <ComboboxInput
          placeholder="Search or create branch..."
          showClear={false}
          showTrigger={false}
          autoFocus
          className="h-5 w-40 text-xs"
          onKeyDown={(e) => {
            if (e.key === "Escape") setOpen(false);
            if (e.key === "Enter" && isNew) {
              e.preventDefault();
              void selectBranch(trimmedInput);
            }
          }}
        />
      </div>
      <ComboboxContent sideOffset={4}>
        <ComboboxList>
          {branches.map((b) => (
            <ComboboxItem key={b} value={b}>
              {b}
            </ComboboxItem>
          ))}
        </ComboboxList>
        {isNew && (
          <div
            role="option"
            className="flex cursor-pointer items-center gap-2 border-t border-border px-2 py-1.5 text-sm hover:bg-accent hover:text-accent-foreground"
            onMouseDown={(e) => {
              e.preventDefault();
              void selectBranch(trimmedInput);
            }}
          >
            <HugeiconsIcon icon={GitBranchIcon} size={14} className="shrink-0 text-muted-foreground" />
            <span>Create <span className="font-medium">{trimmedInput}</span></span>
          </div>
        )}
        <ComboboxEmpty>No branches found.</ComboboxEmpty>
      </ComboboxContent>
    </Combobox>
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
      data-drag-region
      className="flex h-9 select-none items-center border-b border-border/50 bg-background"
    >
      {/* Left: Breadcrumb + Branch */}
      <div className="flex items-center gap-2 pl-3">
        <Breadcrumb />
        <BranchIndicator />
      </div>

      {/* Center: Session indicator */}
      <div data-drag-region className="flex flex-1 items-center justify-center">
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
