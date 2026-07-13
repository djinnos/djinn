import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowDown01Icon,
  CheckmarkCircle04Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import {
  fetchUserConnectedModels,
  saveUserModelSelection,
  type UserModel,
  type UserModelSelection,
} from "@/api/userConfig";
import {
  aggregateModelCapacity,
  defaultLaneMaxSessions,
  MODEL_LANE_KEYS,
  type LaneMaxSessions,
  type ModelLaneKey,
  type ModelLanes,
} from "@/api/userSettings";
import { InlineError } from "@/components/InlineError";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { ConnectedModelPicker } from "@/components/userConfig/ConnectedModelPicker";
import { userConfigKeys } from "@/components/userConfig/userConfigKeys";
import { formatProvider } from "@/components/userConfig/providerDisplay";
import {
  providerDefaultModels,
  sortModels,
  stripProviderPrefix,
} from "@/components/userConfig/modelPicker";

const ROLE_META: Record<
  ModelLaneKey,
  { label: string; description: string }
> = {
  plan: {
    label: "Plan",
    description: "Understands the system and designs the approach.",
  },
  implement: {
    label: "Code",
    description: "Writes and changes code for approved work.",
  },
  review: {
    label: "Review",
    description: "Checks the implementation before it is delivered.",
  },
};

interface OnboardingModelSetupProps {
  targetId: string;
  selection: UserModelSelection;
  onSaved: (selection: UserModelSelection) => void;
}

/**
 * Minimal first-run role assignment. Onboarding chooses exactly one primary
 * model per lane; fallback ordering, concurrency caps, and diversity policy
 * remain available in Settings after setup.
 */
export function OnboardingModelSetup({
  targetId,
  selection,
  onSaved,
}: OnboardingModelSetupProps) {
  const queryClient = useQueryClient();
  const [overrides, setOverrides] = useState<Partial<Record<ModelLaneKey, string>>>(
    {},
  );
  const [laneMaxSessions, setLaneMaxSessions] = useState<LaneMaxSessions>(() =>
    onboardingLaneLimits(selection.laneMaxSessions),
  );

  const connectedModels = useQuery({
    queryKey: userConfigKeys.connectedModels(targetId),
    queryFn: () => fetchUserConnectedModels(targetId),
  });
  const models = useMemo(
    () => sortModels(connectedModels.data ?? []),
    [connectedModels.data],
  );
  const defaultModels = useMemo(
    () => providerDefaultModels(models),
    [models],
  );
  const modelIds = useMemo(
    () => new Set(models.map((model) => model.id)),
    [models],
  );
  const onlyModelId = models.length === 1 ? models[0]?.id : undefined;

  const choices = useMemo(() => {
    const next = {} as Record<ModelLaneKey, string>;
    for (const lane of MODEL_LANE_KEYS) {
      const persisted = selection.lanes[lane][0];
      next[lane] =
        overrides[lane] ??
        (persisted && modelIds.has(persisted) ? persisted : undefined) ??
        onlyModelId ??
        "";
    }
    return next;
  }, [modelIds, onlyModelId, overrides, selection.lanes]);

  const allRolesSelected = MODEL_LANE_KEYS.every((lane) => choices[lane] !== "");

  const saveMutation = useMutation({
    mutationFn: () => {
      const lanes: ModelLanes = {
        plan: withPrimary(choices.plan, selection.lanes.plan, modelIds),
        implement: withPrimary(
          choices.implement,
          selection.lanes.implement,
          modelIds,
        ),
        review: withPrimary(choices.review, selection.lanes.review, modelIds),
      };
      const retainedModelIds = new Set(
        MODEL_LANE_KEYS.flatMap((lane) => lanes[lane]),
      );
      // Caps are a user-owned map, not lane-local state. Preserve every
      // stored entry and only add the default for a newly retained model.
      const maxSessions = { ...selection.maxSessions };
      for (const modelId of retainedModelIds) {
        maxSessions[modelId] ??= 1;
      }
      // A model cap is shared across every lane using that model. Raise it to
      // the combined configured lane capacity so it cannot silently undercut
      // the user's onboarding choices.
      const requiredModelSessions = aggregateModelCapacity(
        lanes,
        laneMaxSessions,
      );
      for (const [modelId, required] of Object.entries(requiredModelSessions)) {
        maxSessions[modelId] = Math.max(maxSessions[modelId] ?? 1, required);
      }
      return saveUserModelSelection(targetId, lanes, maxSessions, {
        laneMaxSessions,
      });
    },
    onSuccess: (saved) => {
      queryClient.setQueryData(userConfigKeys.modelSelection(targetId), saved);
      onSaved(saved);
    },
  });

  if (connectedModels.isError) {
    return (
      <InlineError
        message={
          connectedModels.error instanceof Error
            ? connectedModels.error.message
            : "Failed to load connected models"
        }
        onRetry={() => void connectedModels.refetch()}
      />
    );
  }

  if (connectedModels.isLoading) {
    return (
      <div className="rounded-xl border bg-card/30 p-8 text-center text-sm text-muted-foreground">
        Loading your models…
      </div>
    );
  }

  if (models.length === 0) {
    return (
      <InlineError message="No compatible models are available. Go back and reconnect or refresh your model provider." />
    );
  }

  return (
    <div className="flex flex-col gap-5">
      {onlyModelId && (
        <div className="flex items-start gap-2 rounded-lg border border-primary/25 bg-primary/10 px-4 py-3 text-sm">
          <HugeiconsIcon
            icon={CheckmarkCircle04Icon}
            size={17}
            className="mt-0.5 shrink-0 text-primary"
          />
          <p>
            <span className="font-medium">{modelName(models[0]!)}</span> is your
            available model, so we selected it for all three roles.
          </p>
        </div>
      )}

      <div className="grid gap-3 md:grid-cols-3">
        {MODEL_LANE_KEYS.map((lane) => {
          const meta = ROLE_META[lane];
          const selectedModel = models.find(
            (model) => model.id === choices[lane],
          );
          const selectedModelLabel = selectedModel
            ? `${modelName(selectedModel)} · ${formatProvider(
                selectedModel.provider_id ?? "unknown",
              )}`
            : "Select a model";
          return (
            <div
              key={lane}
              className="flex min-w-0 flex-col gap-3 rounded-xl border bg-card/40 p-4"
            >
              <div>
                <p className="text-sm font-semibold">{meta.label}</p>
                <p className="mt-1 text-xs text-muted-foreground">
                  {meta.description}
                </p>
              </div>
              <ConnectedModelPicker
                models={defaultModels}
                allModels={models}
                title={`Select ${meta.label.toLowerCase()} model`}
                emptyMessage="No models found."
                triggerAriaLabel={`${meta.label} model: ${
                  selectedModel ? selectedModelLabel : "not selected"
                }`}
                triggerVariant="outline"
                triggerSize="lg"
                triggerClassName="h-10 w-full min-w-0 justify-between px-3 font-normal"
                disabled={saveMutation.isPending}
                selectedModelId={selectedModel?.id}
                triggerLabel={
                  <>
                    <span className="min-w-0 truncate">
                      {selectedModelLabel}
                    </span>
                    <HugeiconsIcon
                      icon={ArrowDown01Icon}
                      size={16}
                      className="shrink-0 text-muted-foreground"
                    />
                  </>
                }
                onSelect={(model) =>
                  setOverrides((current) => ({
                    ...current,
                    [lane]: model.id,
                  }))
                }
              />
              <div className="flex items-center justify-between gap-3 border-t pt-3">
                <div className="min-w-0">
                  <Label
                    htmlFor={`onboarding-${lane}-parallel-agents`}
                    className="text-xs font-medium"
                  >
                    Parallel agents
                  </Label>
                  <p className="text-[11px] text-muted-foreground">
                    Maximum running at once
                  </p>
                </div>
                <Select
                  value={String(laneMaxSessions[lane])}
                  onValueChange={(value) =>
                    setLaneMaxSessions((current) => ({
                      ...current,
                      [lane]: Number(value),
                    }))
                  }
                  disabled={saveMutation.isPending}
                >
                  <SelectTrigger
                    id={`onboarding-${lane}-parallel-agents`}
                    aria-label={`${meta.label} parallel agents`}
                    className="h-8 w-[72px]"
                  >
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {[1, 2, 3].map((value) => (
                      <SelectItem key={value} value={String(value)}>
                        {value}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
            </div>
          );
        })}
      </div>

      <p className="text-center text-xs text-muted-foreground">
        Parallel agents is the maximum number Djinn can run at the same time in
        each lane; Code controls workers. Start with 1 (onboarding allows 1–3).
        You can change this later in Settings → Model Roles.
      </p>

      {saveMutation.isError && (
        <InlineError
          message={
            saveMutation.error instanceof Error
              ? saveMutation.error.message
              : "Failed to save model roles"
          }
          onRetry={() => saveMutation.mutate()}
        />
      )}

      <Button
        className="w-full"
        disabled={!allRolesSelected || saveMutation.isPending}
        onClick={() => saveMutation.mutate()}
      >
        {saveMutation.isPending ? "Saving models…" : "Save models and continue"}
      </Button>
    </div>
  );
}

function onboardingLaneLimits(
  persisted: LaneMaxSessions | undefined,
): LaneMaxSessions {
  const defaults = defaultLaneMaxSessions();
  return {
    plan: Math.min(persisted?.plan ?? defaults.plan, 3),
    implement: Math.min(persisted?.implement ?? defaults.implement, 3),
    review: Math.min(persisted?.review ?? defaults.review, 3),
  };
}

function modelName(model: UserModel): string {
  return model.name || stripProviderPrefix(model.id);
}

function withPrimary(
  primary: string,
  existing: string[],
  connectedModelIds: Set<string>,
): string[] {
  return [
    primary,
    ...existing.filter(
      (modelId) => modelId !== primary && connectedModelIds.has(modelId),
    ),
  ];
}
