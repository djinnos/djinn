import { beforeEach, describe, expect, it } from 'vitest';
import { useWizardStore } from './wizardStore';

describe('wizardStore', () => {
  beforeEach(() => {
    useWizardStore.setState({
      currentStep: 1,
      totalSteps: 5,
      completedSteps: [],
      skippedSteps: [],
      isCompleted: false,
      wasSkipped: false,
    });
  });

  it('navigates steps and marks completion path', () => {
    const st = useWizardStore.getState();
    st.nextStep();
    st.prevStep();
    st.goToStep(3);
    expect(useWizardStore.getState().currentStep).toBe(3);
    st.markStepComplete(3);
    expect(useWizardStore.getState().completedSteps).toContain(3);
  });

  it('skips step and can complete/reset wizard', () => {
    const st = useWizardStore.getState();
    st.skipStep();
    expect(useWizardStore.getState().wasSkipped).toBe(true);
    st.completeWizard();
    expect(useWizardStore.getState().isCompleted).toBe(true);
    st.resetWizard();
    expect(useWizardStore.getState().currentStep).toBe(1);
    expect(useWizardStore.getState().isCompleted).toBe(false);
  });
});
