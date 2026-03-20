import { ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { WizardStepIndicator } from "./WizardStepIndicator";
import { useWizardStore } from "@/stores/wizardStore";
import { cn } from "@/lib/utils";

interface WizardProps {
  children: ReactNode;
  onComplete?: () => void;
  onSkip?: () => void;
  className?: string;
}

export function Wizard({
  children,
  onComplete,
  onSkip,
  className,
}: WizardProps) {
  const {
    currentStep,
    totalSteps,
    completedSteps,
    skippedSteps,
    nextStep,
    prevStep,
    skipStep,
    completeWizard,
    isCompleted,
  } = useWizardStore();

  const isLastStep = currentStep === totalSteps;

  const handleNext = () => {
    if (isLastStep) {
      completeWizard();
      onComplete?.();
    } else {
      nextStep();
    }
  };

  const handleSkip = () => {
    skipStep();
    onSkip?.();
  };

  if (isCompleted) {
    return null;
  }

  return (
    <div className={cn("flex min-h-screen flex-col bg-background", className)}>
      <div className="flex flex-1 flex-col">
        <header className="flex items-center justify-between border-b border-border px-6 py-4">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium">Getting Started</span>
          </div>
          <WizardStepIndicator
            currentStep={currentStep}
            totalSteps={totalSteps}
            completedSteps={completedSteps}
            skippedSteps={skippedSteps}
          ></WizardStepIndicator>
          <Button variant="ghost" size="sm" onClick={handleSkip}>
            Skip
          </Button>
        </header>
        <main className="flex flex-1 flex-col items-center justify-center p-6">
          <div className="w-full max-w-md">{children}</div>
        </main>
        <footer className="flex items-center justify-between border-t border-border px-6 py-4">
          <Button
            variant="outline"
            onClick={prevStep}
            disabled={currentStep === 1}
          >
            Back
          </Button>
          <Button onClick={handleNext}>
            {isLastStep ? "Finish" : "Next"}
          </Button>
        </footer>
      </div>
    </div>
  );
}
