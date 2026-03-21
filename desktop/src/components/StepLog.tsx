import { CheckmarkCircle04Icon, CancelCircleIcon, CircleIcon, ZapIcon } from "@hugeicons/core-free-icons";
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
  originalDurationMs?: number;
  emphasizedStepId?: string | null;
  className?: string;
}

function formatDuration(durationMs?: number): string {
  if (typeof durationMs !== "number") return "";
  return `${(durationMs / 1000).toFixed(1)}s`;
}

function getStepValue(index: number): string {
  return `step-${index}`;
}

function getBorderClass(status: StepEntry["status"]): string {
  switch (status) {
    case "passed":
      return "border-emerald-400";
    case "failed":
      return "border-red-400";
    case "running":
      return "border-blue-400";
    case "skipped":
      return "border-zinc-500";
    default:
      return "border-zinc-500";
  }
}

function StepStatusIcon({ stepStatus }: { stepStatus: StepEntry["status"] }) {
  switch (stepStatus) {
    case "running":
      return <Spinner size="xs" className="text-blue-400" />;
    case "passed":
      return <HugeiconsIcon icon={CheckmarkCircle04Icon} size={16} className="text-emerald-400" />;
    case "failed":
      return <HugeiconsIcon icon={CancelCircleIcon} size={16} className="text-red-400" />;
    case "skipped":
      return <HugeiconsIcon icon={CircleIcon} size={16} className="text-zinc-500 [stroke-dasharray:2_2]" />;
    default:
      return <HugeiconsIcon icon={CircleIcon} size={16} className="text-zinc-500" />;
  }
}

export function StepLog({ steps, status, originalDurationMs, emphasizedStepId, className }: StepLogProps) {
  if (steps.length === 0) {
    return (
      <div className={cn("rounded-md border border-border bg-card p-3 text-sm text-muted-foreground", className)}>
        No steps to display.
      </div>
    );
  }

  const failedStepValues = steps
    .filter((step) => step.status === "failed")
    .map((step) => getStepValue(step.index));
  const cacheDuration = formatDuration(originalDurationMs);
  const cacheLabel = cacheDuration ? `Cached (verified recently, ${cacheDuration})` : "Cached (verified recently)";

  return (
    <div className={cn("rounded-md border border-border bg-card p-2", className)}>
      <Accordion defaultValue={failedStepValues} multiple>
        {status === "cache_hit" && (
          <div className="flex items-center gap-2 px-2 py-2 text-sm text-amber-400">
            <HugeiconsIcon icon={ZapIcon} size={16} />
            <span>{cacheLabel}</span>
          </div>
        )}
        {steps.map((step) => {
          const durationLabel = step.status === "skipped" ? "skipped" : formatDuration(step.durationMs);
          const hasOutput = Boolean(step.command || step.stdout || step.stderr);
          const stepValue = getStepValue(step.index);
          const isEmphasized = emphasizedStepId === stepValue;

          return (
            <AccordionItem
              key={step.index}
              value={stepValue}
              className={cn(
                "border-l-2 pl-3 pr-2",
                getBorderClass(step.status),
                step.status === "skipped" && "opacity-60",
                isEmphasized && "bg-red-500/10 ring-1 ring-red-400/60"
              )}
            >
              <AccordionTrigger className="py-2 hover:no-underline">
                <div className="flex w-full items-center gap-2 text-sm">
                  <StepStatusIcon stepStatus={step.status} />
                  <span className={cn("truncate", isEmphasized && "font-semibold text-red-500")}>{step.name}</span>
                  <span className="ml-auto text-muted-foreground">{durationLabel}</span>
                </div>
              </AccordionTrigger>

              {hasOutput && (
                <AccordionContent>
                  <ScrollArea className="max-h-48 rounded-md bg-muted">
                    <pre className="p-3 font-mono text-xs whitespace-pre-wrap break-words">
                      {step.command ? `$ ${step.command}\n` : ""}
                      {step.stdout ?? ""}
                    </pre>
                    {step.stderr && (
                      <pre className="bg-red-400/30 p-3 font-mono text-xs whitespace-pre-wrap break-words text-red-900 dark:text-red-100">
                        {step.stderr}
                      </pre>
                    )}
                  </ScrollArea>
                </AccordionContent>
              )}
            </AccordionItem>
          );
        })}
      </Accordion>
    </div>
  );
}
