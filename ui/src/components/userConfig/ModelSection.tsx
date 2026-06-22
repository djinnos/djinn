import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Reorder, useDragControls } from "framer-motion";
import { Delete02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { InlineError } from "@/components/InlineError";
import {
  ModelSelector,
  ModelSelectorContent,
  ModelSelectorEmpty,
  ModelSelectorGroup,
  ModelSelectorInput,
  ModelSelectorItem,
  ModelSelectorList,
  ModelSelectorLogo,
  ModelSelectorName,
  ModelSelectorSeparator,
  ModelSelectorTrigger,
} from "@/components/ai-elements/model-selector";
import {
  type UserModel,
  fetchUserConnectedModels,
  fetchUserModelSelection,
  saveUserModelSelection,
} from "@/api/userConfig";
import {
  MODEL_LANE_KEYS,
  type ModelLaneKey,
  type ModelLanes,
  emptyLanes,
} from "@/api/userSettings";
import { showToast } from "@/lib/toast";

import { userConfigKeys } from "./userConfigKeys";
import { formatProvider } from "./providerDisplay";
import { pickableModels, stripProviderPrefix } from "./modelPicker";
import {
  PRESETS,
  type PresetKey,
  canEnableDiverseReview,
  lanesForPreset,
  reviewDiversityModelIds,
} from "./presets";
import { cn } from "@/lib/utils";

/** Human-friendly labels + helper copy for each per-role lane. */
const LANE_META: Record<ModelLaneKey, { title: string; roles: string }> = {
  plan: { title: "Plan", roles: "Planner, Architect, Chat" },
  implement: { title: "Implement", roles: "Worker" },
  review: { title: "Review", roles: "Reviewer" },
};

/**
 * Per-user, per-ROLE model lanes editor. Each lane (plan / implement / review)
 * is an ordered fallback list with the shared Reorder drag UI + per-model
 * `Sessions` cap. Caps are per-model and shared across lanes. One Save persists
 * all three lanes + the union of caps.
 */
export function ModelSection({ targetId }: { targetId: string }) {
  const queryClient = useQueryClient();

  const connectedModels = useQuery({
    queryKey: userConfigKeys.connectedModels(targetId),
    queryFn: () => fetchUserConnectedModels(targetId),
  });
  const selection = useQuery({
    queryKey: userConfigKeys.modelSelection(targetId),
    queryFn: () => fetchUserModelSelection(targetId),
  });

  // Local working copy of the per-lane ordered selection + per-model caps,
  // seeded from the server and kept isolated from the current-user settings
  // store. We sync from the server value during render (the React-recommended
  // "adjust state while rendering" pattern) instead of an effect: when the
  // editor has unsaved edits (`dirty`) we hold onto the working copy until
  // they save, otherwise we mirror whatever the server last returned.
  const [lanes, setLanes] = useState<ModelLanes>(emptyLanes);
  const [caps, setCaps] = useState<Record<string, number>>({});
  // Cross-model ("Thorough") review toggle. Defaults ON (server default); the
  // gate below disables interaction when fewer than 2 distinct model ids are
  // reachable by the Implement + Review lanes.
  const [diverseReview, setDiverseReview] = useState(true);
  const [dirty, setDirty] = useState(false);
  const [lastServer, setLastServer] = useState<typeof selection.data>(undefined);

  if (selection.data && selection.data !== lastServer) {
    setLastServer(selection.data);
    if (!dirty) {
      setLanes(selection.data.lanes);
      setCaps(selection.data.maxSessions);
      setDiverseReview(selection.data.diverseReview);
    }
  }

  const modelsById = useMemo(() => {
    const map = new Map<string, UserModel>();
    for (const model of connectedModels.data ?? []) map.set(model.id, model);
    return map;
  }, [connectedModels.data]);

  // The curated picker offers only recommended flagships (with a per-provider
  // fallback to all models when nothing is curated). Already-selected non-
  // flagship picks still render from `modelsById` — this only limits OFFERS.
  const pickable = useMemo(
    () => pickableModels(connectedModels.data ?? []),
    [connectedModels.data],
  );

  // Distinct union of every model id selected across all three lanes.
  const allSelected = useMemo(() => {
    const set = new Set<string>();
    for (const key of MODEL_LANE_KEYS) for (const id of lanes[key]) set.add(id);
    return set;
  }, [lanes]);

  // Cross-model review gate: needs ≥2 distinct model ids reachable by the
  // Implement + Review lanes. Recomputed from the live working copy, so it
  // re-enables automatically the moment a 2nd distinct id is added (no save).
  const reviewDistinctIds = useMemo(() => reviewDiversityModelIds(lanes), [lanes]);
  const diverseReviewEnabled = reviewDistinctIds.size >= 2;
  // The single model id available when the gate is closed (for the hint copy).
  const soleReviewModel = useMemo(() => {
    const [only] = reviewDistinctIds;
    return only ? stripProviderPrefix(only) : undefined;
  }, [reviewDistinctIds]);
  // Effective toggle value: forced off in the UI when the gate is closed, even
  // if the persisted value is true (dispatch already falls back to same-model).
  const effectiveDiverseReview = diverseReview && diverseReviewEnabled;

  const saveMutation = useMutation({
    mutationFn: () => {
      // Only persist caps for models still selected in some lane, default 1.
      const maxSessions: Record<string, number> = {};
      for (const id of allSelected) maxSessions[id] = caps[id] ?? 1;
      return saveUserModelSelection(targetId, lanes, maxSessions, diverseReview);
    },
    onSuccess: (saved) => {
      setLanes(saved.lanes);
      setCaps(saved.maxSessions);
      setDiverseReview(saved.diverseReview);
      setDirty(false);
      queryClient.setQueryData(userConfigKeys.modelSelection(targetId), saved);
      showToast.success("Model roles saved");
    },
    onError: (error) => {
      showToast.error("Could not save model roles", {
        description: error instanceof Error ? error.message : "Unknown error",
      });
    },
  });

  const addModel = (lane: ModelLaneKey, model: UserModel) => {
    setLanes((prev) =>
      prev[lane].includes(model.id)
        ? prev
        : { ...prev, [lane]: [...prev[lane], model.id] },
    );
    setDirty(true);
  };
  const removeModel = (lane: ModelLaneKey, id: string) => {
    setLanes((prev) => ({ ...prev, [lane]: prev[lane].filter((m) => m !== id) }));
    setDirty(true);
  };
  const reorderLane = (lane: ModelLaneKey, next: string[]) => {
    setLanes((prev) => ({ ...prev, [lane]: next }));
    setDirty(true);
  };
  const updateCap = (id: string, value: number) => {
    setCaps((prev) => ({ ...prev, [id]: value }));
    setDirty(true);
  };

  // Apply a working-style preset: overwrite the lanes from the connected models
  // and (for Max quality) force cross-model review ON when ≥2 distinct model
  // ids exist. Lanes stay user-editable afterward — a preset is just a seed.
  const applyPreset = (preset: PresetKey) => {
    const nextLanes = lanesForPreset(preset, connectedModels.data ?? []);
    setLanes(nextLanes);
    if (preset === "maxQuality") {
      // Force ON only in the non-degenerate (≥2 distinct) case; otherwise leave
      // it as-is (dispatch falls back to same-model, surfaced by the hint).
      setDiverseReview(canEnableDiverseReview(nextLanes));
    }
    setDirty(true);
  };

  const toggleDiverseReview = () => {
    if (!diverseReviewEnabled) return; // gated — see hint
    setDiverseReview((prev) => !prev);
    setDirty(true);
  };

  const isLoading = connectedModels.isLoading || selection.isLoading;
  const loadError = connectedModels.error ?? selection.error;

  return (
    <section className="flex flex-col gap-4">
      <div className="flex items-start justify-between gap-4">
        <div>
          <h3 className="text-base font-semibold text-foreground">Model roles</h3>
          <p className="text-sm text-muted-foreground">
            Pick which models run for each role, in priority (fallback) order —
            top runs first. Each lane maps to the agents below it.
          </p>
        </div>
        {dirty && (
          <Button
            variant="outline"
            size="sm"
            className="shrink-0"
            disabled={saveMutation.isPending}
            onClick={() => saveMutation.mutate()}
          >
            {saveMutation.isPending ? "Saving…" : "Save"}
          </Button>
        )}
      </div>

      {loadError ? (
        <InlineError
          message={loadError instanceof Error ? loadError.message : "Failed to load models"}
          onRetry={() => {
            void connectedModels.refetch();
            void selection.refetch();
          }}
        />
      ) : isLoading ? (
        <div className="py-8 text-center text-sm text-muted-foreground">Loading…</div>
      ) : (connectedModels.data?.length ?? 0) === 0 ? (
        <div className="rounded-md border border-dashed p-8 text-center text-sm text-muted-foreground">
          Connect a provider first to unlock models.
        </div>
      ) : (
        <div className="flex flex-col gap-5">
          <PresetBar onApply={applyPreset} />

          <ThoroughReviewToggle
            checked={effectiveDiverseReview}
            enabled={diverseReviewEnabled}
            soleModel={soleReviewModel}
            saving={saveMutation.isPending}
            onToggle={toggleDiverseReview}
          />

          {MODEL_LANE_KEYS.map((lane) => (
            <LaneEditor
              key={lane}
              lane={lane}
              order={lanes[lane]}
              modelsById={modelsById}
              caps={caps}
              availableToAdd={pickable.filter(
                (model) => !lanes[lane].includes(model.id),
              )}
              onAdd={(model) => addModel(lane, model)}
              onRemove={(id) => removeModel(lane, id)}
              onReorder={(next) => reorderLane(lane, next)}
              onUpdateCap={updateCap}
            />
          ))}
        </div>
      )}
    </section>
  );
}

/** The two working-style presets (Balanced / Max quality). */
function PresetBar({ onApply }: { onApply: (preset: PresetKey) => void }) {
  return (
    <div className="flex flex-col gap-2 rounded-lg border bg-card/40 p-4">
      <div>
        <h4 className="text-sm font-semibold text-foreground">Working style</h4>
        <p className="text-xs text-muted-foreground/70">
          A quick start — sets the lanes below from your connected models. You can
          still tweak each lane afterward.
        </p>
      </div>
      <div className="flex flex-wrap gap-2">
        {PRESETS.map((preset) => (
          <Button
            key={preset.key}
            variant="outline"
            size="sm"
            className="h-auto flex-col items-start gap-0.5 px-3 py-2 text-left"
            onClick={() => onApply(preset.key)}
          >
            <span className="font-semibold">{preset.title}</span>
            <span className="text-xs font-normal text-muted-foreground">
              {preset.description}
            </span>
          </Button>
        ))}
      </div>
    </div>
  );
}

/**
 * Cross-model ("Thorough") review toggle. Disabled — with a Connections-tab
 * hint — until ≥2 distinct model ids are reachable by the Implement + Review
 * lanes. Re-enables automatically when a 2nd distinct id appears.
 */
function ThoroughReviewToggle({
  checked,
  enabled,
  soleModel,
  saving,
  onToggle,
}: {
  checked: boolean;
  enabled: boolean;
  soleModel: string | undefined;
  saving: boolean;
  onToggle: () => void;
}) {
  return (
    <div className="flex flex-col gap-2 rounded-lg border bg-card/40 p-4">
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <h4 className="text-sm font-semibold text-foreground">Thorough review</h4>
          <p className="text-xs text-muted-foreground/70">
            Have a different model review the code than the one that wrote it —
            catches more issues by avoiding a single model's blind spots.
          </p>
        </div>
        <button
          type="button"
          role="switch"
          aria-checked={checked}
          aria-label="Thorough review"
          disabled={!enabled || saving}
          onClick={onToggle}
          className={cn(
            "relative inline-flex h-6 w-11 shrink-0 items-center rounded-full border transition-colors",
            checked ? "border-primary bg-primary" : "border-border bg-muted",
            enabled ? "cursor-pointer" : "cursor-not-allowed opacity-50",
            saving && "opacity-60",
          )}
        >
          <span
            className={cn(
              "inline-block h-4 w-4 transform rounded-full bg-white shadow transition-transform",
              checked ? "translate-x-6" : "translate-x-1",
            )}
          />
        </button>
      </div>
      {!enabled && (
        <p className="text-xs text-amber-600 dark:text-amber-500">
          Connect a second model to enable cross-model review
          {soleModel ? ` — only ${soleModel} is available` : ""}. Add another model
          in the{" "}
          <span className="font-medium">Connections</span> tab, then put it in the
          Implement or Review lane.
        </p>
      )}
    </div>
  );
}

function LaneEditor({
  lane,
  order,
  modelsById,
  caps,
  availableToAdd,
  onAdd,
  onRemove,
  onReorder,
  onUpdateCap,
}: {
  lane: ModelLaneKey;
  order: string[];
  modelsById: Map<string, UserModel>;
  caps: Record<string, number>;
  availableToAdd: UserModel[];
  onAdd: (model: UserModel) => void;
  onRemove: (id: string) => void;
  onReorder: (next: string[]) => void;
  onUpdateCap: (id: string, value: number) => void;
}) {
  const meta = LANE_META[lane];
  return (
    <div className="flex flex-col gap-2 rounded-lg border bg-card/40 p-4">
      <div className="flex items-start justify-between gap-4">
        <div>
          <h4 className="text-sm font-semibold text-foreground">{meta.title}</h4>
          <p className="text-xs text-muted-foreground/70">{meta.roles}</p>
        </div>
        <AddModelButton models={availableToAdd} onSelect={onAdd} />
      </div>
      {order.length === 0 ? (
        <div className="rounded-md border border-dashed p-4 text-center text-xs text-muted-foreground">
          No models for this role. Add one — or it falls back to the deployment default.
        </div>
      ) : (
        <Reorder.Group
          axis="y"
          values={order}
          onReorder={onReorder}
          className="space-y-2"
          layoutScroll
        >
          {order.map((modelId) => (
            <ModelRow
              key={modelId}
              modelId={modelId}
              model={modelsById.get(modelId)}
              maxConcurrent={caps[modelId] ?? 1}
              onUpdateCap={(value) => onUpdateCap(modelId, value)}
              onRemove={() => onRemove(modelId)}
            />
          ))}
        </Reorder.Group>
      )}
    </div>
  );
}

export function ModelRow({
  modelId,
  model,
  maxConcurrent,
  onUpdateCap,
  onRemove,
}: {
  modelId: string;
  model: UserModel | undefined;
  maxConcurrent: number;
  onUpdateCap: (value: number) => void;
  onRemove: () => void;
}) {
  const controls = useDragControls();
  const providerId = model?.provider_id ?? modelId.split("/")[0] ?? "unknown";
  const name = model?.name ?? stripProviderPrefix(modelId);

  const [sessionText, setSessionText] = useState(String(maxConcurrent));
  useEffect(() => {
    setSessionText(String(maxConcurrent));
  }, [maxConcurrent]);

  const commitSessions = () => {
    const value = parseInt(sessionText, 10);
    if (!isNaN(value) && value >= 1 && value <= 10) {
      onUpdateCap(value);
      setSessionText(String(value));
    } else {
      setSessionText(String(maxConcurrent));
    }
  };

  return (
    <Reorder.Item value={modelId} dragListener={false} dragControls={controls} className="list-none">
      <div className="flex items-center gap-3 rounded-lg border bg-card px-4 py-3">
        <div
          className="shrink-0 cursor-grab touch-none select-none text-muted-foreground/40 active:cursor-grabbing"
          onPointerDown={(event) => controls.start(event)}
        >
          <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
            <circle cx="9" cy="5" r="1.5" /><circle cx="9" cy="12" r="1.5" /><circle cx="9" cy="19" r="1.5" />
            <circle cx="15" cy="5" r="1.5" /><circle cx="15" cy="12" r="1.5" /><circle cx="15" cy="19" r="1.5" />
          </svg>
        </div>
        <div className="min-w-0 flex-1">
          <div className="truncate font-semibold">{name}</div>
          <div className="text-xs text-muted-foreground/60">{formatProvider(providerId)}</div>
        </div>
        {/* Per-model concurrency cap for the target user. */}
        <div className="flex shrink-0 items-center gap-2">
          <Label className="text-sm text-muted-foreground">Sessions:</Label>
          <Input
            type="text"
            inputMode="numeric"
            value={sessionText}
            onChange={(event) => setSessionText(event.target.value)}
            onBlur={commitSessions}
            onKeyDown={(event) => {
              if (event.key === "Enter") event.currentTarget.blur();
            }}
            className="h-9 w-16 text-center"
          />
        </div>
        <Button
          variant="ghost"
          size="sm"
          onClick={onRemove}
          className="h-8 w-8 shrink-0 p-0 text-destructive hover:bg-destructive/10 hover:text-destructive"
        >
          <HugeiconsIcon icon={Delete02Icon} size={16} />
        </Button>
      </div>
    </Reorder.Item>
  );
}

export function AddModelButton({
  models,
  onSelect,
}: {
  models: UserModel[];
  onSelect: (model: UserModel) => void;
}) {
  const [open, setOpen] = useState(false);

  const groups = useMemo(() => {
    const map = new Map<string, UserModel[]>();
    for (const model of models) {
      const providerId = model.provider_id ?? "unknown";
      if (!map.has(providerId)) map.set(providerId, []);
      map.get(providerId)!.push(model);
    }
    return Array.from(map.entries())
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([provider, items]) => ({
        provider,
        items: items.slice().sort((a, b) => a.name.localeCompare(b.name)),
      }));
  }, [models]);

  return (
    <ModelSelector open={open} onOpenChange={setOpen}>
      <ModelSelectorTrigger render={<Button variant="default" size="sm" />}>
        Add model
      </ModelSelectorTrigger>
      <ModelSelectorContent title="Add a model">
        <ModelSelectorInput placeholder="Search models…" />
        <ModelSelectorList>
          <ModelSelectorEmpty>No connected models available.</ModelSelectorEmpty>
          {groups.map((group, index) => (
            <ModelSelectorGroup key={group.provider} heading={formatProvider(group.provider)}>
              {group.items.map((model) => (
                <ModelSelectorItem
                  key={model.id}
                  searchValue={model.name}
                  onSelect={() => {
                    onSelect(model);
                    setOpen(false);
                  }}
                >
                  <ModelSelectorLogo provider={group.provider} />
                  <ModelSelectorName>{model.name}</ModelSelectorName>
                </ModelSelectorItem>
              ))}
              {index < groups.length - 1 && <ModelSelectorSeparator />}
            </ModelSelectorGroup>
          ))}
        </ModelSelectorList>
      </ModelSelectorContent>
    </ModelSelector>
  );
}
