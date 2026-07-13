import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type RefObject,
} from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowRight01Icon,
  CheckmarkCircle04Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import logoSvg from "@/assets/logo.svg";
import { Button } from "@/components/ui/button";
import { InlineError } from "@/components/InlineError";
import { cn } from "@/lib/utils";
import {
  ApiKeyConnectForm,
  CodexConnectCard,
} from "@/components/userConfig/ProviderSection";
import { userConfigKeys } from "@/components/userConfig/userConfigKeys";
import {
  SELF_TARGET,
  fetchUserCatalog,
  fetchUserConnectedProviders,
  fetchUserModelSelection,
  type UserModelSelection,
} from "@/api/userConfig";
import {
  MODEL_LANE_KEYS,
  type ModelLanes,
} from "@/api/userSettings";

import { OnboardingModelSetup } from "./OnboardingModelSetup";
import { OnboardingProgress } from "./OnboardingProgress";

type StepKey = "connect" | "models";

/**
 * First-run onboarding sheet. A focused, sequential flow for a brand-new user
 * with no connected providers and/or no model-role assignments:
 *
 *   1. Connect a subscription (Codex OAuth) or paste an API key.
 *   2. Assign one primary connected model to Plan, Code (`implement` on the
 *      backend), and Review through the focused first-run selector.
 *
 * Rendered by `AuthenticatedApp` in place of the legacy provider/model gates.
 * Saving the required model roles calls `onFinished`, which refreshes the
 * server-backed gates and advances the outer onboarding flow.
 */
export function FirstRunOnboarding({
  onFinished,
}: {
  onFinished: () => void;
}) {
  const queryClient = useQueryClient();
  const [stepOverride, setStepOverride] = useState<StepKey | null>(null);

  const connectedProviders = useQuery({
    queryKey: userConfigKeys.connectedProviders(SELF_TARGET),
    queryFn: () => fetchUserConnectedProviders(SELF_TARGET),
  });
  const catalog = useQuery({
    queryKey: userConfigKeys.catalog(SELF_TARGET),
    queryFn: () => fetchUserCatalog(SELF_TARGET),
  });
  const modelSelection = useQuery({
    queryKey: userConfigKeys.modelSelection(SELF_TARGET),
    queryFn: () => fetchUserModelSelection(SELF_TARGET),
  });

  const connectedCount = connectedProviders.data?.length ?? 0;
  const hasProvider = connectedCount > 0;
  const codexConnected = (connectedProviders.data ?? []).some(
    (p) => p.id === "openai" && p.connection_methods.includes("oauth"),
  );
  const connectedIds = useMemo(
    () => new Set((connectedProviders.data ?? []).map((p) => p.id)),
    [connectedProviders.data],
  );
  // Resume at the first incomplete substep. A connected provider means the
  // user should land directly on role selection after a refresh.
  const activeStep: StepKey = stepOverride ?? (hasProvider ? "models" : "connect");
  const stepHeadingRef = useRef<HTMLHeadingElement>(null);
  const providerLoadError = connectedProviders.error ?? catalog.error;

  useEffect(() => {
    stepHeadingRef.current?.focus();
  }, [activeStep]);

  const refreshProviders = () => {
    void queryClient.invalidateQueries({
      queryKey: userConfigKeys.connectedProviders(SELF_TARGET),
    });
    void queryClient.invalidateQueries({
      queryKey: userConfigKeys.connectedModels(SELF_TARGET),
    });
  };

  return (
    <main className="flex min-h-screen flex-col items-center justify-center bg-background px-6 py-12 text-foreground">
      <div
        className={cn(
          "flex w-full flex-col items-center gap-8",
          activeStep === "models" ? "max-w-6xl" : "max-w-xl",
        )}
      >
        <div className="relative">
          <div
            className="pointer-events-none absolute left-1/2 top-1/2 h-16 w-16 -translate-x-1/2 -translate-y-1/2 rounded-full bg-purple-400/40"
            style={{ filter: "blur(40px)" }}
          />
          <img
            src={logoSvg}
            alt="Djinn"
            className="relative h-16 w-auto drop-shadow-[0_0_40px_rgba(168,139,250,0.35)]"
          />
        </div>

        <OnboardingProgress current="models" />

        <div className="w-full">
          {activeStep === "connect" && (
            <ConnectStep
              loading={connectedProviders.isLoading || catalog.isLoading}
              error={
                providerLoadError
                  ? providerLoadError instanceof Error
                    ? providerLoadError.message
                    : "Failed to load provider setup"
                  : null
              }
              onRetry={() => {
                void connectedProviders.refetch();
                void catalog.refetch();
              }}
              headingRef={stepHeadingRef}
              hasProvider={hasProvider}
              connectedCount={connectedCount}
              codexConnected={codexConnected}
              catalog={catalog.data ?? []}
              connectedIds={connectedIds}
              onConnected={refreshProviders}
              onContinue={() => setStepOverride("models")}
            />
          )}

          {activeStep === "models" && (
            <ModelsStep
              selection={modelSelection.data}
              laneLocked={modelSelection.data?.laneLocked === true}
              loading={modelSelection.isLoading}
              error={modelSelection.error}
              onRetry={() => void modelSelection.refetch()}
              headingRef={stepHeadingRef}
              onBack={() => setStepOverride("connect")}
              onContinue={onFinished}
            />
          )}
        </div>
      </div>
    </main>
  );
}

function ModelsStep({
  selection,
  laneLocked,
  loading,
  error,
  onRetry,
  headingRef,
  onBack,
  onContinue,
}: {
  selection: UserModelSelection | undefined;
  laneLocked: boolean;
  loading: boolean;
  error: unknown;
  onRetry: () => void;
  headingRef: RefObject<HTMLHeadingElement | null>;
  onBack: () => void;
  onContinue: () => void;
}) {
  const hasEveryRole = hasEveryModelRole(selection?.lanes);

  return (
    <div className="flex flex-col gap-5">
      <div className="text-center">
        <h1
          ref={headingRef}
          tabIndex={-1}
          className="text-xl font-semibold outline-none"
        >
          Assign models to roles
        </h1>
        <p className="mt-1 text-sm text-muted-foreground">
          Choose the primary model Djinn uses to plan, write code, and review
          each change. You can tune advanced behavior later in Settings.
        </p>
      </div>

      {error ? (
        <InlineError
          message={error instanceof Error ? error.message : "Failed to load model roles"}
          onRetry={onRetry}
        />
      ) : loading || !selection ? (
        <div className="rounded-xl border bg-card/30 p-8 text-center text-sm text-muted-foreground">
          Loading your model roles…
        </div>
      ) : laneLocked && hasEveryRole ? (
        <div className="rounded-xl border bg-card/30 p-6 text-center">
          <h2 className="text-sm font-semibold">Managed by your organization</h2>
          <p className="mt-1 text-sm text-muted-foreground">
            Your organization&apos;s AI policy supplies the model roles. You can
            continue with those inherited settings.
          </p>
        </div>
      ) : laneLocked ? (
        <div className="rounded-xl border border-amber-500/30 bg-amber-500/10 p-6 text-center">
          <h2 className="text-sm font-semibold">Model roles need an administrator</h2>
          <p className="mt-1 text-sm text-muted-foreground">
            Your organization manages model roles, but Plan, Code, and Review
            have not been assigned. Ask an administrator to configure the AI
            policy before continuing.
          </p>
        </div>
      ) : (
        <OnboardingModelSetup
          targetId={SELF_TARGET}
          selection={selection}
          onSaved={onContinue}
        />
      )}

      <div className="flex items-center justify-between">
        <Button variant="ghost" size="sm" onClick={onBack}>
          Back
        </Button>
        {laneLocked && hasEveryRole && (
          <Button
            size="sm"
            onClick={onContinue}
          >
            Continue
            <HugeiconsIcon icon={ArrowRight01Icon} size={15} />
          </Button>
        )}
      </div>
    </div>
  );
}

function ConnectStep({
  loading,
  error,
  onRetry,
  headingRef,
  hasProvider,
  connectedCount,
  codexConnected,
  catalog,
  connectedIds,
  onConnected,
  onContinue,
}: {
  loading: boolean;
  error: string | null;
  onRetry: () => void;
  headingRef: RefObject<HTMLHeadingElement | null>;
  hasProvider: boolean;
  connectedCount: number;
  codexConnected: boolean;
  catalog: Parameters<typeof ApiKeyConnectForm>[0]["catalog"];
  connectedIds: Set<string>;
  onConnected: () => void;
  onContinue: () => void;
}) {
  return (
    <div className="flex flex-col gap-5">
      <div className="text-center">
        <h1
          ref={headingRef}
          tabIndex={-1}
          className="text-xl font-semibold outline-none"
        >
          Connect a model provider
        </h1>
        <p className="mt-1 text-sm text-muted-foreground">
          Djinn runs your agents on your own subscription or API key. Sign in
          with ChatGPT, or paste an API key — you can add more later.
        </p>
      </div>

      {error ? (
        <InlineError message={error} onRetry={onRetry} />
      ) : (
        <>
          {hasProvider && (
            <div className="flex items-center justify-center gap-2 rounded-lg border border-green-500/30 bg-green-500/10 px-4 py-2 text-sm text-green-600 dark:text-green-500">
              <HugeiconsIcon icon={CheckmarkCircle04Icon} size={15} />
              {connectedCount} provider{connectedCount === 1 ? "" : "s"} connected
            </div>
          )}

          <CodexConnectCard
            targetId={SELF_TARGET}
            alreadyConnected={codexConnected}
            onConnected={onConnected}
          />

          <ApiKeyConnectForm
            targetId={SELF_TARGET}
            catalog={catalog}
            connectedIds={connectedIds}
            catalogLoading={loading}
            onConnected={onConnected}
          />
        </>
      )}

      <div className="flex items-center justify-end">
        <Button
          size="sm"
          disabled={!hasProvider}
          onClick={onContinue}
          title={hasProvider ? undefined : "Connect a provider to continue"}
        >
          Continue
          <HugeiconsIcon icon={ArrowRight01Icon} size={15} />
        </Button>
      </div>
    </div>
  );
}

function hasEveryModelRole(lanes: ModelLanes | undefined): boolean {
  return MODEL_LANE_KEYS.every((lane) => (lanes?.[lane].length ?? 0) > 0);
}
