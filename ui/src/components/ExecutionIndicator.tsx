import { useEffect, useRef, useState } from 'react';

import { useExecutionStatus } from '@/hooks/useExecutionStatus';

function formatModelName(modelId: string): string {
  // "openai/gpt-5.3-codex" → "gpt-5.3-codex"
  const parts = modelId.split('/');
  return parts.length > 1 ? parts.slice(1).join('/') : modelId;
}

/**
 * Compact session/capacity indicator. Renders a `running/max sessions`
 * trigger; clicking opens a per-model capacity popover.
 *
 * Returns null when execution status hasn't reported yet so the host
 * (Sidebar footer) can collapse cleanly.
 */
export function ExecutionIndicator() {
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
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [open]);

  if (state === null) return null;

  const capacityEntries = Object.entries(capacity);

  return (
    <div ref={ref} className="relative w-full">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center justify-between gap-2 rounded-md px-3 py-1.5 text-xs font-medium text-muted-foreground transition-colors hover:bg-white/[0.04] hover:text-foreground outline-none"
      >
        <span>Sessions</span>
        <span className="tabular-nums">
          {runningSessions}/{maxSessions}
        </span>
      </button>
      {open && (
        <div className="absolute bottom-full left-0 right-0 z-50 mb-1 min-w-[200px] rounded-lg bg-popover p-1 text-popover-foreground shadow-md ring-1 ring-foreground/10 animate-in fade-in-0 zoom-in-95">
          <div className="px-2 py-1 text-xs font-medium text-muted-foreground">Capacity</div>
          <div className="-mx-1 my-1 h-px bg-border" />
          {capacityEntries.length === 0 ? (
            <div className="px-2 py-1.5 text-xs text-muted-foreground">No models configured</div>
          ) : (
            capacityEntries.map(([modelId, cap]) => (
              <div
                key={modelId}
                className="flex items-center justify-between gap-4 px-2 py-1.5 text-xs"
              >
                <span className="truncate text-muted-foreground">{formatModelName(modelId)}</span>
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
