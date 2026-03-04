import { ReactNode } from "react";
import { useWizardStore } from "@/stores/wizardStore";

interface WizardStepProps {
  stepNumber: number;
  children: ReactNode;
}

export function WizardStep({ stepNumber, children }: WizardStepProps) {
  const { currentStep } = useWizardStore();

  if (currentStep !== stepNumber) {
    return null;
  }

  return <div className="animate-in fade-in duration-300">{children}</div>;
}
