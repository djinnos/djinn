import { CheckmarkCircle04Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { cn } from "@/lib/utils";

const STEPS = [
  { key: "repository", label: "Repository" },
  { key: "models", label: "Models" },
  { key: "environment", label: "Environment" },
] as const;

export type OnboardingProgressStep = (typeof STEPS)[number]["key"];

export function OnboardingProgress({
  current,
  complete = false,
}: {
  current: OnboardingProgressStep;
  complete?: boolean;
}) {
  const currentIndex = complete
    ? STEPS.length
    : STEPS.findIndex((step) => step.key === current);
  const currentLabel = complete
    ? "Onboarding complete"
    : `Step ${currentIndex + 1} of ${STEPS.length}: ${STEPS[currentIndex]?.label}`;

  return (
    <div>
      <p role="status" aria-live="polite" aria-atomic="true" className="sr-only">
        {currentLabel}
      </p>
      <ol
        aria-label="Onboarding progress"
        className="flex max-w-full items-center justify-center gap-2"
      >
        {STEPS.map((step, index) => {
          const done = index < currentIndex;
          const active = index === currentIndex;
          return (
            <li key={step.key} className="flex min-w-0 items-center gap-2">
              <span
                aria-current={active ? "step" : undefined}
                className={cn(
                  "flex h-7 w-7 shrink-0 items-center justify-center rounded-full border text-xs font-semibold",
                  active && "border-primary bg-primary text-primary-foreground",
                  done && "border-primary bg-primary/15 text-primary",
                  !active && !done && "border-border text-muted-foreground",
                )}
              >
                {done ? (
                  <HugeiconsIcon icon={CheckmarkCircle04Icon} size={15} />
                ) : (
                  index + 1
                )}
              </span>
              <span
                className={cn(
                  "hidden text-xs sm:inline",
                  active ? "font-medium text-foreground" : "text-muted-foreground",
                )}
              >
                {step.label}
              </span>
              {index < STEPS.length - 1 && (
                <span className="h-px w-5 shrink-0 bg-border sm:w-8" aria-hidden />
              )}
            </li>
          );
        })}
      </ol>
    </div>
  );
}
