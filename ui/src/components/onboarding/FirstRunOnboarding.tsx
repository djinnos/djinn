import { useMemo, useState } from "react";
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
import { ModelSection } from "@/components/userConfig/ModelSection";
import { userConfigKeys } from "@/components/userConfig/userConfigKeys";
import {
  SELF_TARGET,
  fetchUserCatalog,
  fetchUserConnectedProviders,
  fetchUserModelSelection,
} from "@/api/userConfig";
import {
  MODEL_LANE_KEYS,
  type ModelLanes,
} from "@/api/userSettings";

import { dismissFirstRun } from "./firstRun";

type StepKey = "connect" | "models" | "done";

const STEPS: { key: StepKey; label: string }[] = [
  { key: "connect", label: "Connect" },
  { key: "models", label: "Models" },
  { key: "done", label: "Done" },
];

/**
 * First-run onboarding sheet. A focused, sequential flow for a brand-new user
 * with no connected providers and/or no model-role assignments:
 *
 *   1. Connect a subscription (Codex OAuth) or paste an API key. Skippable.
 *   2. Assign at least one connected model to Plan, Implement, and Review.
 *      The production Model Roles editor is embedded here so onboarding and
 *      Settings cannot drift.
 *   3. Done — finish.
 *
 * Rendered by `AuthenticatedApp` in place of the legacy provider/model gates.
 * On finish/skip it calls `onFinished` (which refreshes the server gates and
 * records a client-side dismissal so it doesn't re-appear on this device).
 */
export function FirstRunOnboarding({
  userId,
  onFinished,
}: {
  userId: string | null;
  onFinished: () => void;
}) {
  const queryClient = useQueryClient();
  const [step, setStep] = useState<StepKey>("connect");

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
  const modelRolesConfigured =
    modelSelection.data?.laneLocked === true ||
    hasEveryModelRole(modelSelection.data?.lanes);

  const refreshProviders = () => {
    void queryClient.invalidateQueries({
      queryKey: userConfigKeys.connectedProviders(SELF_TARGET),
    });
    void queryClient.invalidateQueries({
      queryKey: userConfigKeys.connectedModels(SELF_TARGET),
    });
  };

  const finish = () => {
    dismissFirstRun(userId);
    onFinished();
  };

  return (
    <main className="flex min-h-screen flex-col items-center justify-center bg-background px-6 py-12 text-foreground">
      <div
        className={cn(
          "flex w-full flex-col items-center gap-8",
          step === "models" ? "max-w-6xl" : "max-w-xl",
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

        <Stepper current={step} />

        <div className="w-full">
          {step === "connect" && (
            <ConnectStep
              loading={connectedProviders.isLoading || catalog.isLoading}
              error={
                connectedProviders.isError
                  ? connectedProviders.error instanceof Error
                    ? connectedProviders.error.message
                    : "Failed to load providers"
                  : null
              }
              onRetry={() => void connectedProviders.refetch()}
              hasProvider={hasProvider}
              connectedCount={connectedCount}
              codexConnected={codexConnected}
              catalog={catalog.data ?? []}
              connectedIds={connectedIds}
              onConnected={refreshProviders}
              onSkip={finish}
              onContinue={() => setStep("models")}
            />
          )}

          {step === "models" && (
            <ModelsStep
              lanes={modelSelection.data?.lanes}
              laneLocked={modelSelection.data?.laneLocked === true}
              loading={modelSelection.isLoading}
              error={modelSelection.error}
              onRetry={() => void modelSelection.refetch()}
              onBack={() => setStep("connect")}
              onSkip={() => setStep("done")}
              onContinue={() => setStep("done")}
            />
          )}

          {step === "done" && (
            <DoneStep
              modelRolesConfigured={modelRolesConfigured}
              onFinish={finish}
            />
          )}
        </div>
      </div>
    </main>
  );
}

function ModelsStep({
  lanes,
  laneLocked,
  loading,
  error,
  onRetry,
  onBack,
  onSkip,
  onContinue,
}: {
  lanes: ModelLanes | undefined;
  laneLocked: boolean;
  loading: boolean;
  error: unknown;
  onRetry: () => void;
  onBack: () => void;
  onSkip: () => void;
  onContinue: () => void;
}) {
  const hasEveryRole = laneLocked || hasEveryModelRole(lanes);

  return (
    <div className="flex flex-col gap-5">
      <div className="text-center">
        <h2 className="text-xl font-semibold">Assign models to roles</h2>
        <p className="mt-1 text-sm text-muted-foreground">
          Choose at least one model for Plan, Implement, and Review, then save.
          The first model in each lane runs first; the rest are fallbacks.
        </p>
      </div>

      {error ? (
        <InlineError
          message={error instanceof Error ? error.message : "Failed to load model roles"}
          onRetry={onRetry}
        />
      ) : laneLocked ? (
        <div className="rounded-xl border bg-card/30 p-6 text-center">
          <h3 className="text-sm font-semibold">Managed by your organization</h3>
          <p className="mt-1 text-sm text-muted-foreground">
            Your organization&apos;s AI policy supplies the model roles. You can
            continue with those inherited settings.
          </p>
        </div>
      ) : (
        <div className="rounded-xl border bg-card/30 p-5">
          <ModelSection targetId={SELF_TARGET} onboarding />
        </div>
      )}

      <div className="flex items-center justify-between">
        <Button variant="ghost" size="sm" onClick={onBack}>
          Back
        </Button>
        <div className="flex items-center gap-2">
          <Button variant="outline" size="sm" onClick={onSkip}>
            Skip for now
          </Button>
          <Button
            size="sm"
            disabled={loading || !hasEveryRole}
            onClick={onContinue}
            title={
              hasEveryRole
                ? undefined
                : "Save at least one model in Plan, Implement, and Review"
            }
          >
            Continue
            <HugeiconsIcon icon={ArrowRight01Icon} size={15} />
          </Button>
        </div>
      </div>
    </div>
  );
}

function Stepper({ current }: { current: StepKey }) {
  const currentIndex = STEPS.findIndex((s) => s.key === current);
  return (
    <ol className="flex items-center gap-2">
      {STEPS.map((s, i) => {
        const done = i < currentIndex;
        const active = i === currentIndex;
        return (
          <li key={s.key} className="flex items-center gap-2">
            <span
              className={cn(
                "flex h-6 w-6 items-center justify-center rounded-full border text-xs font-semibold",
                active && "border-primary bg-primary text-primary-foreground",
                done && "border-primary bg-primary/15 text-primary",
                !active && !done && "border-border text-muted-foreground",
              )}
            >
              {done ? <HugeiconsIcon icon={CheckmarkCircle04Icon} size={14} /> : i + 1}
            </span>
            <span
              className={cn(
                "text-xs",
                active ? "font-medium text-foreground" : "text-muted-foreground",
              )}
            >
              {s.label}
            </span>
            {i < STEPS.length - 1 && (
              <span className="mx-1 h-px w-6 bg-border" aria-hidden />
            )}
          </li>
        );
      })}
    </ol>
  );
}

function ConnectStep({
  loading,
  error,
  onRetry,
  hasProvider,
  connectedCount,
  codexConnected,
  catalog,
  connectedIds,
  onConnected,
  onSkip,
  onContinue,
}: {
  loading: boolean;
  error: string | null;
  onRetry: () => void;
  hasProvider: boolean;
  connectedCount: number;
  codexConnected: boolean;
  catalog: Parameters<typeof ApiKeyConnectForm>[0]["catalog"];
  connectedIds: Set<string>;
  onConnected: () => void;
  onSkip: () => void;
  onContinue: () => void;
}) {
  return (
    <div className="flex flex-col gap-5">
      <div className="text-center">
        <h2 className="text-xl font-semibold">Connect a model provider</h2>
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

      <div className="flex items-center justify-between">
        <Button variant="ghost" size="sm" onClick={onSkip}>
          Skip for now
        </Button>
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

function DoneStep({
  modelRolesConfigured,
  onFinish,
}: {
  modelRolesConfigured: boolean;
  onFinish: () => void;
}) {
  return (
    <div className="flex flex-col items-center gap-5 text-center">
      <div className="flex h-12 w-12 items-center justify-center rounded-full bg-primary/15">
        <HugeiconsIcon icon={CheckmarkCircle04Icon} size={26} className="text-primary" />
      </div>
      <div>
        <h2 className="text-xl font-semibold">You&apos;re all set</h2>
        <p className="mt-1 text-sm text-muted-foreground">
          {modelRolesConfigured
            ? "Your provider and model roles are ready. You can change them any time in Settings → Model Roles."
            : "Your provider is connected. Add role-specific models later in Settings → Model Roles."}
        </p>
      </div>

      <Button className="px-8" onClick={onFinish}>
        Get started
      </Button>
    </div>
  );
}
