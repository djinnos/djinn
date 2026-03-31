import { CheckmarkCircle04Icon, CancelCircleIcon, CircleIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Accordion, AccordionContent, AccordionItem, AccordionTrigger } from "@/components/ui/accordion";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Spinner } from "@/components/ui/spinner";
import { cn } from "@/lib/utils";
import type { StepEntry, VerificationRun } from "@/stores/verificationStore";

type RunStatus = VerificationRun["status"];

interface StepLogProps {
  steps: StepEntry[];
  status: RunStatus;
  label?: string;
  originalDurationMs?: number;
  emphasizedStepId?: string | null;
  className?: string;
}

function formatDuration(durationMs?: number): string {
  if (typeof durationMs !== "number") return "";
  if (durationMs < 1000) return `${durationMs}ms`;
  return `${(durationMs / 1000).toFixed(1)}s`;
}

function getStepValue(index: number): string {
  return `step-${index}`;
}

// ── Status icon ─────────────────────────────────────────────────────────────

function StepStatusIcon({ stepStatus }: { stepStatus: StepEntry["status"] }) {
  switch (stepStatus) {
    case "running":
      return <Spinner size="xs" className="text-blue-400" />;
    case "passed":
      return <HugeiconsIcon icon={CheckmarkCircle04Icon} size={16} className="text-emerald-400" />;
    case "failed":
      return <HugeiconsIcon icon={CancelCircleIcon} size={16} className="text-red-400" />;
    case "skipped":
      return <HugeiconsIcon icon={CircleIcon} size={16} className="text-amber-400" />;
    default:
      return <HugeiconsIcon icon={CircleIcon} size={16} className="text-zinc-500" />;
  }
}

// ── Summary header with pill badges + progress ──────────────────────────────

function StepSummary({ steps, status, originalDurationMs }: { steps: StepEntry[]; status: RunStatus; originalDurationMs?: number }) {
  const passed = steps.filter((s) => s.status === "passed").length;
  const failed = steps.filter((s) => s.status === "failed").length;
  const skipped = steps.filter((s) => s.status === "skipped").length;
  const running = steps.filter((s) => s.status === "running").length;
  const total = steps.length;
  const completed = passed + failed + skipped;
  const totalDuration = steps.reduce((sum, s) => sum + (s.durationMs ?? 0), 0);
  const passRate = total > 0 ? Math.round((passed / total) * 100) : 0;

  return (
    <div className="space-y-3 px-3 pt-3 pb-2">
      {/* Status pills + duration */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          {passed > 0 && (
            <span className="inline-flex items-center gap-1.5 rounded-full bg-emerald-500/15 px-2.5 py-1 text-xs font-medium text-emerald-400">
              <HugeiconsIcon icon={CheckmarkCircle04Icon} size={14} />
              {passed} passed
            </span>
          )}
          {failed > 0 && (
            <span className="inline-flex items-center gap-1.5 rounded-full bg-red-500/15 px-2.5 py-1 text-xs font-medium text-red-400">
              <HugeiconsIcon icon={CancelCircleIcon} size={14} />
              {failed} failed
            </span>
          )}
          {skipped > 0 && (
            <span className="inline-flex items-center gap-1.5 rounded-full bg-amber-500/15 px-2.5 py-1 text-xs font-medium text-amber-400">
              <HugeiconsIcon icon={CircleIcon} size={14} />
              {skipped} skipped
            </span>
          )}
          {running > 0 && (
            <span className="inline-flex items-center gap-1.5 rounded-full bg-blue-500/15 px-2.5 py-1 text-xs font-medium text-blue-400">
              <Spinner size="xs" />
              {running} running
            </span>
          )}
        </div>
        <span className="text-sm text-muted-foreground">
          {status === "cache_hit"
            ? `cached${originalDurationMs ? ` (${formatDuration(originalDurationMs)})` : ""}`
            : totalDuration > 0
              ? formatDuration(totalDuration)
              : ""}
        </span>
      </div>

      {/* Progress bar + count */}
      {total > 0 && (
        <div className="space-y-1.5">
          <div className="flex h-2 overflow-hidden rounded-full bg-muted">
            {passed > 0 && (
              <div className="bg-emerald-400 transition-all duration-300" style={{ width: `${(passed / total) * 100}%` }} />
            )}
            {failed > 0 && (
              <div className="bg-red-400 transition-all duration-300" style={{ width: `${(failed / total) * 100}%` }} />
            )}
            {running > 0 && (
              <div className="animate-pulse bg-blue-400 transition-all duration-300" style={{ width: `${(running / total) * 100}%` }} />
            )}
            {skipped > 0 && (
              <div className="bg-amber-400/50 transition-all duration-300" style={{ width: `${(skipped / total) * 100}%` }} />
            )}
            {completed < total && running === 0 && (
              <div className="bg-zinc-700" style={{ width: `${((total - completed) / total) * 100}%` }} />
            )}
          </div>
          <div className="flex items-center justify-between text-xs text-muted-foreground">
            <span>{passed}/{total} steps passed</span>
            <span>{passRate}%</span>
          </div>
        </div>
      )}
    </div>
  );
}

// ── Main component ──────────────────────────────────────────────────────────

export function StepLog({ steps, status, label, originalDurationMs, emphasizedStepId, className }: StepLogProps) {
  if (steps.length === 0) {
    return (
      <div className={cn("rounded-lg border border-border bg-card p-3 text-sm text-muted-foreground", className)}>
        No steps to display.
      </div>
    );
  }

  const failedStepValues = steps
    .filter((step) => step.status === "failed")
    .map((step) => getStepValue(step.index));

  return (
    <div className={cn("overflow-hidden rounded-lg border border-border bg-card", className)}>
      {label && (
        <div className="px-3 pt-3 pb-0">
          <span className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">{label}</span>
        </div>
      )}
      <StepSummary steps={steps} status={status} originalDurationMs={originalDurationMs} />
      <div className="px-2 pb-2">
        <Accordion defaultValue={failedStepValues} multiple>
          {steps.map((step) => {
            const durationLabel = step.status === "skipped" ? "skipped" : formatDuration(step.durationMs);
            const hasOutput = Boolean(step.command || step.stdout || step.stderr);
            const stepValue = getStepValue(step.index);
            const isEmphasized = emphasizedStepId === stepValue;

            if (!hasOutput) {
              return (
                <div
                  key={step.index}
                  className={cn(
                    "flex items-center gap-2.5 rounded-md border border-transparent px-3 py-2.5 text-sm",
                    isEmphasized && "border-red-500/40 bg-red-500/5",
                  )}
                >
                  <StepStatusIcon stepStatus={step.status} />
                  <span className={cn(
                    "truncate font-medium",
                    step.status === "skipped" && "text-muted-foreground",
                    isEmphasized && "text-red-400",
                  )}>
                    {step.name}
                  </span>
                  <span className="ml-auto text-xs text-muted-foreground">{durationLabel}</span>
                </div>
              );
            }

            return (
              <AccordionItem
                key={step.index}
                value={stepValue}
                className={cn(
                  "rounded-md border border-transparent px-3",
                  isEmphasized && "border-red-500/40 bg-red-500/5",
                )}
              >
                <AccordionTrigger className="py-2.5 hover:no-underline">
                  <div className="flex w-full items-center gap-2.5 text-sm">
                    <StepStatusIcon stepStatus={step.status} />
                    <span className={cn(
                      "truncate font-medium",
                      step.status === "skipped" && "text-muted-foreground",
                      isEmphasized && "text-red-400",
                    )}>
                      {step.name}
                    </span>
                    <span className="ml-auto text-xs text-muted-foreground">{durationLabel}</span>
                  </div>
                </AccordionTrigger>

                <AccordionContent>
                  {step.stderr && (
                    <div className="mb-2 rounded-md bg-red-500/10 px-3 py-2.5">
                      <pre className="font-mono text-xs text-red-300 whitespace-pre-wrap break-words">
                        {step.stderr}
                      </pre>
                    </div>
                  )}
                  {(step.command || step.stdout) && (
                    <ScrollArea className="max-h-48 rounded-md bg-muted/50">
                      <pre className="p-3 font-mono text-xs text-foreground/70 whitespace-pre-wrap break-words">
                        {step.command ? `$ ${step.command}\n` : ""}
                        {step.stdout ?? ""}
                      </pre>
                    </ScrollArea>
                  )}
                </AccordionContent>
              </AccordionItem>
            );
          })}
        </Accordion>
      </div>
    </div>
  );
}
