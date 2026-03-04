import { create } from 'zustand';
import { persist } from 'zustand/middleware';

export interface WizardState {
  currentStep: number;
  totalSteps: number;
  completedSteps: number[];
  skippedSteps: number[];
  isCompleted: boolean;
  wasSkipped: boolean;
}

export interface WizardActions {
  goToStep: (step: number) => void;
  nextStep: () => void;
  prevStep: () => void;
  skipStep: () => void;
  markStepComplete: (step: number) => void;
  completeWizard: () => void;
  resetWizard: () => void;
  canSkip: () => boolean;
}

const INITIAL_STATE: WizardState = {
  currentStep: 1,
  totalSteps: 4,
  completedSteps: [],
  skippedSteps: [],
  isCompleted: false,
  wasSkipped: false,
};

export const useWizardStore = create<WizardState & WizardActions>()(
  persist(
    (set, get) => ({
      ...INITIAL_STATE,

      goToStep: (step: number) => {
        const { totalSteps } = get();
        if (step >= 1 && step <= totalSteps) {
          set({ currentStep: step });
        }
      },

      nextStep: () => {
        const { currentStep, totalSteps, completedSteps } = get();
        if (currentStep < totalSteps) {
          set({
            currentStep: currentStep + 1,
            completedSteps: [...new Set([...completedSteps, currentStep])],
          });
        }
      },

      prevStep: () => {
        const { currentStep } = get();
        if (currentStep > 1) {
          set({ currentStep: currentStep - 1 });
        }
      },

      skipStep: () => {
        const { currentStep, totalSteps, skippedSteps } = get();
        if (currentStep < totalSteps) {
          set({
            currentStep: currentStep + 1,
            skippedSteps: [...new Set([...skippedSteps, currentStep])],
            wasSkipped: true,
          });
        } else {
          set({
            skippedSteps: [...new Set([...skippedSteps, currentStep])],
            isCompleted: true,
            wasSkipped: true,
          });
        }
      },

      markStepComplete: (step: number) => {
        const { completedSteps } = get();
        set({
          completedSteps: [...new Set([...completedSteps, step])],
        });
      },

      completeWizard: () => {
        const { currentStep, completedSteps } = get();
        set({
          isCompleted: true,
          completedSteps: [...new Set([...completedSteps, currentStep])],
        });
      },

      resetWizard: () => {
        set(INITIAL_STATE);
      },

      canSkip: () => {
        return true;
      },
    }),
    {
      name: 'djinnos-wizard-storage',
      partialize: (state) => ({
        currentStep: state.currentStep,
        totalSteps: state.totalSteps,
        completedSteps: state.completedSteps,
        skippedSteps: state.skippedSteps,
        isCompleted: state.isCompleted,
        wasSkipped: state.wasSkipped,
      }),
    }
  )
);

export const shouldShowWizard = (): boolean => {
  const stored = localStorage.getItem('djinnos-wizard-storage');
  if (stored) {
    const state = JSON.parse(stored);
    if (!state.state?.isCompleted) {
      return true;
    }
    return false;
  }
  return true;
};
