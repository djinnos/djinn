import { useCallback, useEffect, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import {
  type Agent,
  type LearnedPromptAmendment,
  type LearnedPromptHistory,
  clearLearnedPrompt,
  fetchLearnedPromptHistory,
} from "@/api/agents";
import { ConfirmButton } from "@/components/ConfirmButton";
import { cn } from "@/lib/utils";

function formatMetricDelta(before: number, after: number): string {
  const delta = after - before;
  const sign = delta >= 0 ? "+" : "";
  return `${sign}${delta.toFixed(2)}`;
}

function AmendmentEntry({ amendment }: { amendment: LearnedPromptAmendment }) {
  const HIDDEN_METRICS = new Set(["agent_name", "completed_task_count"]);
  const metricKeys = Object.keys(amendment.metrics_before).filter((key) => {
    if (HIDDEN_METRICS.has(key)) return false;
    const before = Number(amendment.metrics_before[key]);
    const after = Number(amendment.metrics_after[key]);
    return !Number.isNaN(before) && !Number.isNaN(after);
  });

  return (
    <div className="rounded-lg border border-border/40 p-3 space-y-2 text-xs">
      <div className="flex items-center gap-2">
        <span className="text-muted-foreground">
          {new Date(amendment.created_at).toLocaleString()}
        </span>
        {amendment.metrics_after.completed_task_count != null && (
          <span className="text-muted-foreground/60 font-mono">
            {Number(amendment.metrics_after.completed_task_count)} tasks
          </span>
        )}
      </div>

      <div className="prose prose-sm max-w-none dark:prose-invert text-xs leading-relaxed">
        <ReactMarkdown remarkPlugins={[remarkGfm]}>
          {amendment.proposed_text}
        </ReactMarkdown>
      </div>

      {metricKeys.length > 0 && (
        <div className="flex flex-wrap gap-3 text-muted-foreground">
          {metricKeys.map((key) => {
            const before = Number(amendment.metrics_before[key] ?? 0);
            const after = Number(amendment.metrics_after[key] ?? 0);
            const delta = after - before;
            return (
              <span key={key}>
                {key}:{" "}
                <span className="text-foreground font-mono">
                  {before.toFixed(2)} → {after.toFixed(2)}
                </span>{" "}
                <span
                  className={cn(
                    "font-mono",
                    delta > 0
                      ? "text-green-600 dark:text-green-400"
                      : delta < 0
                        ? "text-red-600 dark:text-red-400"
                        : "text-muted-foreground",
                  )}
                >
                  ({formatMetricDelta(before, after)})
                </span>
              </span>
            );
          })}
        </div>
      )}
    </div>
  );
}

function DiscardedSection({ discarded }: { discarded: LearnedPromptAmendment[] }) {
  const [open, setOpen] = useState(false);

  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex items-center gap-1.5 text-xs text-muted-foreground hover:text-foreground transition-colors"
      >
        Discarded ({discarded.length})
        <span className="text-muted-foreground/60">{open ? "▴" : "▾"}</span>
      </button>
      {open && (
        <div className="mt-1 space-y-4">
          {discarded.map((amendment) => (
            <AmendmentEntry key={amendment.id} amendment={amendment} />
          ))}
        </div>
      )}
    </div>
  );
}

interface LearnedPromptSectionProps {
  role: Agent;
  onCleared?: () => void;
  canClear?: boolean;
}

export function LearnedPromptSection({
  role,
  onCleared,
  canClear = false,
}: LearnedPromptSectionProps) {
  const [expanded, setExpanded] = useState(false);
  const [history, setHistory] = useState<LearnedPromptHistory | null>(null);
  const [loadingHistory, setLoadingHistory] = useState(false);
  const [historyError, setHistoryError] = useState<string | null>(null);
  const [clearing, setClearing] = useState(false);

  const loadHistory = useCallback(async () => {
    setLoadingHistory(true);
    setHistoryError(null);
    try {
      const data = await fetchLearnedPromptHistory(role.id);
      setHistory(data);
    } catch (err) {
      setHistoryError(err instanceof Error ? err.message : "Failed to load history");
    } finally {
      setLoadingHistory(false);
    }
  }, [role.id]);

  useEffect(() => {
    if (expanded && !history && !loadingHistory) {
      void loadHistory();
    }
  }, [expanded, history, loadingHistory, loadHistory]);

  const handleClear = async () => {
    setClearing(true);
    try {
      await clearLearnedPrompt(role.id);
      setHistory(null);
      setExpanded(false);
      onCleared?.();
    } catch (err) {
      setHistoryError(err instanceof Error ? err.message : "Failed to clear learned prompt");
    } finally {
      setClearing(false);
    }
  };

  const hasLearnedPrompt = role.learned_prompt !== null;

  return (
    <div className="border-t border-border pt-3 mt-3 space-y-3">
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0 space-y-1">
          <div className="flex items-center gap-1.5 text-xs font-medium text-foreground">
            <span
              className={cn(
                "rounded-full w-1.5 h-1.5 shrink-0",
                hasLearnedPrompt ? "bg-blue-500" : "bg-muted-foreground/40",
              )}
            />
            Machine-managed learned prompt
            {hasLearnedPrompt && (
              <span className="text-blue-600 dark:text-blue-400">(active)</span>
            )}
          </div>
          <p className="text-xs text-muted-foreground">
            Generated from agent outcomes and shown read-only. Edit human-authored
            system prompt extensions separately.
          </p>
        </div>

        {hasLearnedPrompt && canClear && (
          <ConfirmButton
            title="Clear learned prompt"
            description={`Clear the learned prompt for "${role.name}"? The auto-improvement history will be preserved.`}
            confirmLabel="Clear"
            onConfirm={() => void handleClear()}
            size="sm"
            variant="ghost"
            disabled={clearing}
          >
            {clearing ? "Clearing..." : "Clear"}
          </ConfirmButton>
        )}
      </div>

      {hasLearnedPrompt && (
        <div
          aria-label="Current machine-managed learned prompt"
          className="rounded-lg border border-blue-500/20 bg-blue-500/5 p-3 text-xs leading-relaxed"
        >
          <div className="prose prose-sm max-w-none dark:prose-invert text-xs leading-relaxed">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{role.learned_prompt}</ReactMarkdown>
          </div>
        </div>
      )}

      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        className="flex items-center gap-1.5 text-xs text-muted-foreground hover:text-foreground transition-colors"
      >
        {expanded ? "Hide learned prompt history" : "Show learned prompt history"}
        <span className="text-muted-foreground/60">{expanded ? "▴" : "▾"}</span>
      </button>

      {expanded && (
        <div className="mt-3 space-y-3">
          {loadingHistory && (
            <p className="text-xs text-muted-foreground">Loading history...</p>
          )}
          {historyError && (
            <p className="text-xs text-red-500">{historyError}</p>
          )}
          {history && !loadingHistory && (() => {
            const kept = [...history.amendments]
              .filter((a) => a.action === "keep")
              .sort((a, b) => new Date(a.created_at).getTime() - new Date(b.created_at).getTime());
            const discarded = [...history.amendments]
              .filter((a) => a.action !== "keep")
              .sort((a, b) => new Date(b.created_at).getTime() - new Date(a.created_at).getTime());

            if (kept.length === 0 && discarded.length === 0) {
              return <p className="text-xs text-muted-foreground italic">No amendments yet.</p>;
            }

            return (
              <>
                {kept.length > 0 && (
                  <div>
                    <p className="text-xs font-medium text-muted-foreground mb-1">
                      Kept ({kept.length})
                    </p>
                    <div className="space-y-4">
                      {kept.map((amendment) => (
                        <AmendmentEntry key={amendment.id} amendment={amendment} />
                      ))}
                    </div>
                  </div>
                )}
                {discarded.length > 0 && (
                  <DiscardedSection discarded={discarded} />
                )}
              </>
            );
          })()}
        </div>
      )}
    </div>
  );
}
