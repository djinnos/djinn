import type { ReactNode } from "react";

import logoSvg from "@/assets/logo.svg";
import {
  OnboardingProgress,
  type OnboardingProgressStep,
} from "@/components/onboarding/OnboardingProgress";

export function OnboardingShell({
  children,
  current,
  complete = false,
}: {
  children: ReactNode;
  current: OnboardingProgressStep;
  complete?: boolean;
}) {
  return (
    <main className="flex min-h-screen flex-col items-center justify-center bg-background px-4 py-8 text-foreground sm:px-6 sm:py-12">
      <div className="flex w-full max-w-2xl flex-col items-center gap-7 sm:gap-8">
        <div className="relative">
          <div
            className="pointer-events-none absolute left-1/2 top-1/2 h-16 w-16 -translate-x-1/2 -translate-y-1/2 rounded-full bg-purple-400/40"
            style={{ filter: "blur(40px)" }}
          />
          <img
            src={logoSvg}
            alt="Djinn"
            className="relative h-14 w-auto drop-shadow-[0_0_40px_rgba(168,139,250,0.35)] sm:h-16"
          />
        </div>
        <OnboardingProgress current={current} complete={complete} />
        <div className="w-full">{children}</div>
      </div>
    </main>
  );
}
