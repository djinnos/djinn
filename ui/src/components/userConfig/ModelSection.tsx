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
import {
  formatModelMetadata,
  groupModelsByProvider,
  providerDefaultModels,
  stripProviderPrefix,
} from "./modelPicker";
import { cn } from "@/lib/utils";

/**
 * Distinct model ids reachable by the Implement + Review lanes. "Distinct" is by
 * model id, NOT provider — one provider can host many models. This is the set
 * the cross-model ("Thorough") review gate counts.
 */
function reviewDiversityModelIds(lanes: ModelLanes): Set<string> {
  const ids = new Set<string>();
  for (const id of lanes.implement) ids.add(id);
  for (const id of lanes.review) ids.add(id);
  return ids;
}

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
  // Cross-model ("Diverse") refinement toggle. Defaults ON (server default);
  // the gate below disables interaction when fewer than 2 distinct model ids
  // are reachable across the Plan + Implement lanes (the refinement roles
  // draw from these).
  const [diverseRefinement, setDiverseRefinement] = useState(true);
  const [dirty, setDirty] = useState(false);
  const [lastServer, setLastServer] = useState<typeof selection.data>(undefined);

  if (selection.data && selection.data !== lastServer) {
    setLastServer(selection.data);
    if (!dirty) {
      setLanes(selection.data.lanes);
      setCaps(selection.data.maxSessions);
      setDiverseReview(selection.data.diverseReview);
      setDiverseRefinement(selection.data.diverseRefinement);
    }
  }

  const modelsById = useMemo(() => {
    const map = new Map<string, UserModel>();
    for (const model of connectedModels.data ?? []) map.set(model.id, model);
    return map;
  }, [connectedModels.data]);

  // The default picker view offers recommended flagships (with a per-provider
  // fallback to all models when nothing is curated). The full connected list is
  // still passed into each lane picker for Browse all + search.
  const defaultPickable = useMemo(
    () => providerDefaultModels(connectedModels.data ?? []),
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

  // Cross-model refinement gate: needs ≥2 distinct model ids reachable by the
  // Plan + Implement lanes (the refinement roles draw from these). Same
  // gate-and-hint pattern as the review toggle above.
  const refinementDistinctIds = useMemo(() => {
    const ids = new Set<string>();
    for (const id of lanes.plan) ids.add(id);
    for (const id of lanes.implement) ids.add(id);
    return ids;
  }, [lanes]);
  const diverseRefinementEnabled = refinementDistinctIds.size >= 2;
  const soleRefinementModel = useMemo(() => {
    const [only] = refinementDistinctIds;
    return only ? stripProviderPrefix(only) : undefined;
  }, [refinementDistinctIds]);
  const effectiveDiverseRefinement = diverseRefinement && diverseRefinementEnabled;

  const saveMutation = useMutation({
    mutationFn: () => {
      // Only persist caps for models still selected in some lane, default 1.
      const maxSessions: Record<string, number> = {};
      for (const id of allSelected) maxSessions[id] = caps[id] ?? 1;
      return saveUserModelSelection(targetId, lanes, maxSessions, diverseReview, diverseRefinement);
    },
    onSuccess: (saved) => {
      setLanes(saved.lanes);
      setCaps(saved.maxSessions);
      setDiverseReview(saved.diverseReview);
      setDiverseRefinement(saved.diverseRefinement);
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

  const toggleDiverseReview = () => {
    if (!diverseReviewEnabled) return; // gated — see hint
    setDiverseReview((prev) => !prev);
    setDirty(true);
  };

  const toggleDiverseRefinement = () => {
    if (!diverseRefinementEnabled) return; // gated — see hint
    setDiverseRefinement((prev) => !prev);
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
        <div className="flex items-center gap-2">
          {!loadError && !isLoading && (
            <Button
              variant="outline"
              size="sm"
              className="shrink-0"
              disabled={connectedModels.isFetching || saveMutation.isPending}
              onClick={() => void connectedModels.refetch()}
            >
              {connectedModels.isFetching ? "Refreshing…" : "Refresh models"}
            </Button>
          )}
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
          <ThoroughReviewToggle
            checked={effectiveDiverseReview}
            enabled={diverseReviewEnabled}
            soleModel={soleReviewModel}
            saving={saveMutation.isPending}
            onToggle={toggleDiverseReview}
          />

          <DiverseRefinementToggle
            checked={effectiveDiverseRefinement}
            enabled={diverseRefinementEnabled}
            soleModel={soleRefinementModel}
            saving={saveMutation.isPending}
            onToggle={toggleDiverseRefinement}
          />

          {MODEL_LANE_KEYS.map((lane) => (
            <LaneEditor
              key={lane}
              lane={lane}
              order={lanes[lane]}
              modelsById={modelsById}
              caps={caps}
              availableToAdd={defaultPickable.filter(
                (model) => !lanes[lane].includes(model.id),
              )}
              allAvailableToAdd={(connectedModels.data ?? []).filter(
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

/**
 * Cross-model ("Diverse") refinement toggle for proposal-refinement roles
 * (Advocate, Adversary, Judge). Disabled — with a hint — until ≥2 distinct
 * model ids are reachable by the Plan + Implement lanes. Explains that the
 * refinement roles use best-effort cross-model diversity and collapse to
 * same-model rather than blocking when alternatives are unavailable.
 */
function DiverseRefinementToggle({
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
          <h4 className="text-sm font-semibold text-foreground">Diverse refinement</h4>
          <p className="text-xs text-muted-foreground/70">
            Have Advocate, Adversary, and Judge use a different model than the
            primary task model — uses best-effort cross-model diversity. Falls
            back to same-model when alternatives are unavailable, rather than
            blocking.
          </p>
        </div>
        <button
          type="button"
          role="switch"
          aria-checked={checked}
          aria-label="Diverse refinement"
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
          Connect a second model to enable cross-model refinement
          {soleModel ? ` — only ${soleModel} is available` : ""}. Add another model
          in the{" "}
          <span className="font-medium">Connections</span> tab, then put it in the
          Plan or Implement lane. Refinement roles will collapse to same-model
          when alternatives are unavailable.
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
  allAvailableToAdd,
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
  allAvailableToAdd: UserModel[];
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
        <AddModelButton
          models={availableToAdd}
          allModels={allAvailableToAdd}
          onSelect={onAdd}
        />
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
  allModels,
  onSelect,
}: {
  models: UserModel[];
  allModels?: UserModel[];
  onSelect: (model: UserModel) => void;
}) {
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");
  const [expandedProviders, setExpandedProviders] = useState<Set<string>>(() => new Set());

  const defaultModelIds = useMemo(
    () => new Set(models.map((model) => model.id)),
    [models],
  );
  const searchableModels = allModels ?? models;
  const normalizedSearch = search.trim().toLowerCase();

  const groups = useMemo(() => {
    const source = normalizedSearch
      ? searchableModels.filter((model) => modelMatchesSearch(model, normalizedSearch))
      : searchableModels;

    return groupModelsByProvider(source)
      .map((group) => {
        const hiddenCount = group.models.filter(
          (model) => !defaultModelIds.has(model.id),
        ).length;
        const expanded = expandedProviders.has(group.providerId);
        const items = normalizedSearch || expanded
          ? group.models
          : group.models.filter((model) => defaultModelIds.has(model.id));
        return { ...group, hiddenCount, items };
      })
      .filter((group) => group.items.length > 0 || group.hiddenCount > 0);
  }, [defaultModelIds, expandedProviders, normalizedSearch, searchableModels]);

  return (
    <ModelSelector
      open={open}
      onOpenChange={(nextOpen) => {
        setOpen(nextOpen);
        if (!nextOpen) {
          setSearch("");
          setExpandedProviders(new Set());
        }
      }}
    >
      <ModelSelectorTrigger render={<Button variant="default" size="sm" />}>
        Add model
      </ModelSelectorTrigger>
      <ModelSelectorContent title="Add a model">
        <ModelSelectorInput
          placeholder="Search models…"
          onInputCapture={(event) => setSearch(event.currentTarget.value)}
        />
        <ModelSelectorList>
          <ModelSelectorEmpty>No connected models available.</ModelSelectorEmpty>
          {groups.map((group, index) => (
            <ModelSelectorGroup key={group.providerId} heading={formatProvider(group.providerId)}>
              {group.items.map((model) => (
                <ModelSelectorItem
                  key={model.id}
                  searchValue={modelSearchValue(model)}
                  onSelect={() => {
                    onSelect(model);
                    setOpen(false);
                    setSearch("");
                    setExpandedProviders(new Set());
                  }}
                >
                  <ModelSelectorLogo provider={group.providerId} />
                  <ModelSelectorName>{model.name || stripProviderPrefix(model.id)}</ModelSelectorName>
                  {model.recommended && (
                    <span className="rounded-full bg-primary/10 px-1.5 py-0.5 text-[10px] font-medium text-primary">
                      Recommended
                    </span>
                  )}
                  <span className="ml-auto text-xs text-muted-foreground">
                    {formatModelMetadata(model)}
                  </span>
                </ModelSelectorItem>
              ))}
              {!normalizedSearch &&
                !expandedProviders.has(group.providerId) &&
                group.hiddenCount > 0 && (
                  <button
                    type="button"
                    className="w-full rounded-sm px-2 py-1.5 text-left text-xs font-medium text-primary hover:bg-accent"
                    onClick={() =>
                      setExpandedProviders((prev) => {
                        const next = new Set(prev);
                        next.add(group.providerId);
                        return next;
                      })
                    }
                  >
                    Browse all {formatProvider(group.providerId)} models ({group.hiddenCount} more)
                  </button>
                )}
              {index < groups.length - 1 && <ModelSelectorSeparator />}
            </ModelSelectorGroup>
          ))}
        </ModelSelectorList>
      </ModelSelectorContent>
    </ModelSelector>
  );
}

function modelSearchValue(model: UserModel): string {
  const providerId = model.provider_id ?? "unknown";
  return [model.name, model.id, stripProviderPrefix(model.id), providerId, formatProvider(providerId)]
    .filter(Boolean)
    .join(" ");
}

function modelMatchesSearch(model: UserModel, normalizedSearch: string): boolean {
  return modelSearchValue(model).toLowerCase().includes(normalizedSearch);
}
