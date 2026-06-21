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

/** Strips the provider prefix from a model id ("openai/gpt-5" → "gpt-5"). */
export function stripProviderPrefix(modelId: string): string {
  const slash = modelId.indexOf("/");
  return slash >= 0 ? modelId.slice(slash + 1) : modelId;
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
  const [dirty, setDirty] = useState(false);
  const [lastServer, setLastServer] = useState<typeof selection.data>(undefined);

  if (selection.data && selection.data !== lastServer) {
    setLastServer(selection.data);
    if (!dirty) {
      setLanes(selection.data.lanes);
      setCaps(selection.data.maxSessions);
    }
  }

  const modelsById = useMemo(() => {
    const map = new Map<string, UserModel>();
    for (const model of connectedModels.data ?? []) map.set(model.id, model);
    return map;
  }, [connectedModels.data]);

  // Distinct union of every model id selected across all three lanes.
  const allSelected = useMemo(() => {
    const set = new Set<string>();
    for (const key of MODEL_LANE_KEYS) for (const id of lanes[key]) set.add(id);
    return set;
  }, [lanes]);

  const saveMutation = useMutation({
    mutationFn: () => {
      // Only persist caps for models still selected in some lane, default 1.
      const maxSessions: Record<string, number> = {};
      for (const id of allSelected) maxSessions[id] = caps[id] ?? 1;
      return saveUserModelSelection(targetId, lanes, maxSessions);
    },
    onSuccess: (saved) => {
      setLanes(saved.lanes);
      setCaps(saved.maxSessions);
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
          {MODEL_LANE_KEYS.map((lane) => (
            <LaneEditor
              key={lane}
              lane={lane}
              order={lanes[lane]}
              modelsById={modelsById}
              caps={caps}
              availableToAdd={(connectedModels.data ?? []).filter(
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
