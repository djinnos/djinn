import { cn } from "@/lib/utils";

interface WizardStepIndicatorProps {
  currentStep: number;
  totalSteps: number;
  completedSteps: number[];
  skippedSteps: number[];
}

export function WizardStepIndicator({
  currentStep,
  totalSteps,
  completedSteps,
  skippedSteps,
}: WizardStepIndicatorProps) {
  return (
    <div className="flex flex-col items-center gap-3">
      <span className="text-xs text-muted-foreground">
        Step {currentStep} of {totalSteps}
      </span>
      <div className="flex items-center gap-1.5">
        {Array.from({ length: totalSteps }, (_, i) => {
          const stepNum = i + 1;
          const isActive = stepNum === currentStep;
          const isCompleted = completedSteps.includes(stepNum);
          const isSkipped = skippedSteps.includes(stepNum);

          return (
            <div
              key={stepNum}
              className={cn(
                "h-1.5 w-6 rounded-full transition-colors duration-200",
                isActive && "bg-primary",
                isCompleted && !isSkipped && "bg-primary/60",
                isSkipped && "bg-muted-foreground/40",
                !isActive && !isCompleted && !isSkipped && "bg-muted"
              )}
            ></div>
          );
        })}
      </div>
    </div>
  );
}
