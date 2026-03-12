import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { Wizard } from "@/components/Wizard";
import { WizardStep } from "@/components/WizardStep";
import { useWizardStore } from "@/stores/wizardStore";

describe("Wizard navigation", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useWizardStore.getState().resetWizard();
    useWizardStore.setState({ totalSteps: 2, currentStep: 1, isCompleted: false });
  });

  it("navigates steps and marks completion", () => {
    const onComplete = vi.fn();

    render(
      <Wizard onComplete={onComplete}>
        <WizardStep stepNumber={1}><div>Step 1 content</div></WizardStep>
        <WizardStep stepNumber={2}><div>Step 2 content</div></WizardStep>
      </Wizard>
    );

    fireEvent.click(screen.getByRole("button", { name: "Next" }));
    expect(useWizardStore.getState().currentStep).toBe(2);

    fireEvent.click(screen.getByRole("button", { name: "Finish" }));
    expect(useWizardStore.getState().isCompleted).toBe(true);
    expect(onComplete).toHaveBeenCalled();
  });
});
