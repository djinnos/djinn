import { Loading02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import logoSvg from "@/assets/logo.svg";
import { Button } from "@/components/ui/button";

export function OnboardingGateStatus({
  error,
  onRetry,
}: {
  error?: string | null;
  onRetry?: () => void;
}) {
  return (
    <main className="flex min-h-screen items-center justify-center bg-background px-6 text-foreground">
      <div className="flex max-w-md flex-col items-center gap-5 text-center">
        <img src={logoSvg} alt="Djinn" className="h-16 w-auto" />
        {error ? (
          <>
            <div>
              <h1 className="text-xl font-semibold">Couldn&apos;t check your setup</h1>
              <p className="mt-1 text-sm text-muted-foreground">{error}</p>
            </div>
            <Button onClick={onRetry}>Retry</Button>
          </>
        ) : (
          <div role="status" className="flex items-center gap-2 text-sm text-muted-foreground">
            <HugeiconsIcon icon={Loading02Icon} size={18} className="animate-spin" />
            Checking your setup…
          </div>
        )}
      </div>
    </main>
  );
}
