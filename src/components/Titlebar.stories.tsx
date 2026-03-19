import { useState, useRef, useEffect } from "react";
import type { Meta, StoryObj } from "@storybook/react";
import {
  Cancel01Icon,
  MinusSignIcon,
  SquareIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { ChevronRight, Layers } from "lucide-react";

/* ---------------------------------------------------------------------------
 * TitlebarButton — identical to Titlebar.tsx
 * --------------------------------------------------------------------------- */

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

/* ---------------------------------------------------------------------------
 * ExecutionIndicatorMock — reproduces ExecutionIndicator without hooks
 * --------------------------------------------------------------------------- */

function formatModelName(modelId: string): string {
  const parts = modelId.split("/");
  return parts.length > 1 ? parts.slice(1).join("/") : modelId;
}

function ExecutionIndicatorMock({
  sessions,
  maxSessions,
  capacity,
}: {
  sessions: number;
  maxSessions: number;
  capacity: Record<string, { active: number; max: number }>;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

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

  const capacityEntries = Object.entries(capacity);

  return (
    <div ref={ref} className="relative">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex items-center gap-1.5 rounded px-2 py-0.5 text-xs font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground outline-none"
      >
        <span className="tabular-nums">
          {sessions}/{maxSessions}
        </span>
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

/* ---------------------------------------------------------------------------
 * TitlebarMock — reproduces the full Titlebar layout without Tauri/store hooks
 * --------------------------------------------------------------------------- */

interface TitlebarMockProps {
  projectName: string;
  isAllProjects?: boolean;
  viewLabel?: string;
  sessions: number;
  maxSessions: number;
  capacity: Record<string, { active: number; max: number }>;
}

function TitlebarMock({
  projectName,
  isAllProjects = false,
  viewLabel,
  sessions,
  maxSessions,
  capacity,
}: TitlebarMockProps) {
  return (
    <div
      data-tauri-drag-region
      className="flex h-9 select-none items-center border-b border-border/50 bg-background"
    >
      {/* Left: Breadcrumb */}
      <div className="flex items-center pl-3">
        <div className="flex items-center gap-1.5 text-xs font-medium text-foreground/80">
          {isAllProjects && (
            <Layers className="h-3 w-3 shrink-0 text-muted-foreground" />
          )}
          <span className="max-w-[160px] truncate">{projectName}</span>
          {viewLabel && (
            <>
              <ChevronRight className="h-3 w-3 shrink-0 text-muted-foreground/50" />
              <span className="text-muted-foreground">{viewLabel}</span>
            </>
          )}
        </div>
      </div>

      {/* Center: Session indicator */}
      <div
        data-tauri-drag-region
        className="flex flex-1 items-center justify-center"
      >
        <ExecutionIndicatorMock
          sessions={sessions}
          maxSessions={maxSessions}
          capacity={capacity}
        />
      </div>

      {/* Right: Window controls */}
      <div className="flex h-full items-center">
        <TitlebarButton onClick={() => {}} label="Minimize">
          <HugeiconsIcon
            icon={MinusSignIcon}
            size={14}
            className="pointer-events-none"
          />
        </TitlebarButton>
        <TitlebarButton onClick={() => {}} label="Maximize">
          <HugeiconsIcon
            icon={SquareIcon}
            size={12}
            className="pointer-events-none"
          />
        </TitlebarButton>
        <TitlebarButton onClick={() => {}} label="Close" variant="close">
          <HugeiconsIcon
            icon={Cancel01Icon}
            size={14}
            className="pointer-events-none"
          />
        </TitlebarButton>
      </div>
    </div>
  );
}

/* ---------------------------------------------------------------------------
 * Storybook meta & stories
 * --------------------------------------------------------------------------- */

const meta: Meta = {
  title: "Chrome/Titlebar",
  parameters: {
    layout: "fullscreen",
  },
};
export default meta;

export const Default: StoryObj = {
  render: () => (
    <TitlebarMock
      projectName="DjinnOS Desktop"
      viewLabel="Kanban"
      sessions={2}
      maxSessions={6}
      capacity={{
        "anthropic/claude-sonnet-4-20250514": { active: 2, max: 6 },
      }}
    />
  ),
};

export const AllProjects: StoryObj = {
  render: () => (
    <TitlebarMock
      projectName="All Projects"
      isAllProjects
      viewLabel="Kanban"
      sessions={5}
      maxSessions={8}
      capacity={{
        "anthropic/claude-sonnet-4-20250514": { active: 3, max: 4 },
        "openai/gpt-5.3-codex": { active: 2, max: 4 },
      }}
    />
  ),
};

export const ChatView: StoryObj = {
  render: () => (
    <TitlebarMock
      projectName="DjinnOS Desktop"
      viewLabel="Chat"
      sessions={0}
      maxSessions={6}
      capacity={{
        "anthropic/claude-sonnet-4-20250514": { active: 0, max: 6 },
      }}
    />
  ),
};

export const NoSessions: StoryObj = {
  render: () => (
    <TitlebarMock
      projectName="My Project"
      sessions={0}
      maxSessions={0}
      capacity={{}}
    />
  ),
};
