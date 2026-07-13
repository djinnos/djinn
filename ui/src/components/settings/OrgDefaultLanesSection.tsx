import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Delete02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { Button } from "@/components/ui/button";
import { InlineError } from "@/components/InlineError";
import { ConnectedModelPicker } from "@/components/userConfig/ConnectedModelPicker";
import { stripProviderPrefix } from "@/components/userConfig/modelPicker";
import { formatProvider } from "@/components/userConfig/providerDisplay";
import { userConfigKeys } from "@/components/userConfig/userConfigKeys";
import {
  type UserModel,
  SELF_TARGET,
  fetchUserConnectedModels,
} from "@/api/userConfig";
import {
  MODEL_LANE_KEYS,
  type ModelLaneKey,
  type ModelLanes,
} from "@/api/userSettings";
import type { LockLevel, OrgPolicy } from "@/api/orgPolicy";
import { cn } from "@/lib/utils";

/** Human-friendly labels + the agents each org-default lane drives. */
const LANE_META: Record<ModelLaneKey, { title: string; roles: string }> = {
  plan: { title: "Plan", roles: "Planner, Architect, Chat" },
  implement: { title: "Implement", roles: "Worker" },
  review: { title: "Review", roles: "Reviewer" },
};

interface Props {
  policy: OrgPolicy;
  saving: boolean;
  onSave: (patch: { defaultLanes?: ModelLanes; lockLevel?: LockLevel }) => void;
}

/**
 * Admin-only org-default role-assignment editor. Sets the plan / implement /
 * review lanes that seed every member (and, when the lock below is on, that
 * members can't override). Persists via `org_policy_set`'s `default_lanes`.
 *
 * The model picker is seeded from the admin's own connected models — the
 * realistic universe an admin chooses an org default from. It reuses the Model
 * Roles tab's `ConnectedModelPicker`; the rest is a minimal ordered list (priority
 * high → low per lane) so it stays independent of the per-user lane editor.
 */
export function OrgDefaultLanesSection({ policy, saving, onSave }: Props) {
  const connectedModels = useQuery({
    queryKey: userConfigKeys.connectedModels(SELF_TARGET),
    queryFn: () => fetchUserConnectedModels(SELF_TARGET),
  });

  // Working copy seeded from the server, isolated until Save. Re-seed when the
  // server value changes and we have no unsaved edits.
  const [lanes, setLanes] = useState<ModelLanes>(policy.defaultLanes);
  const [dirty, setDirty] = useState(false);
  const [lastServer, setLastServer] = useState<ModelLanes>(policy.defaultLanes);
  if (policy.defaultLanes !== lastServer) {
    setLastServer(policy.defaultLanes);
    if (!dirty) setLanes(policy.defaultLanes);
  }

  const modelsById = useMemo(() => {
    const map = new Map<string, UserModel>();
    for (const model of connectedModels.data ?? []) map.set(model.id, model);
    return map;
  }, [connectedModels.data]);

  const locked = policy.lockLevel === "locked";

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
  const move = (lane: ModelLaneKey, index: number, dir: -1 | 1) => {
    setLanes((prev) => {
      const order = [...prev[lane]];
      const next = index + dir;
      if (next < 0 || next >= order.length) return prev;
      [order[index], order[next]] = [order[next], order[index]];
      return { ...prev, [lane]: order };
    });
    setDirty(true);
  };

  const save = () => {
    onSave({ defaultLanes: lanes });
    setDirty(false);
  };

  return (
    <section className="flex flex-col gap-3 border-t border-border pt-6">
      <div className="flex items-start justify-between gap-4">
        <div>
          <h2 className="text-xl font-bold text-foreground">Org-default model roles</h2>
          <p className="text-sm text-muted-foreground">
            The plan / implement / review lanes new members start with (priority
            high → low per lane).{" "}
            {locked
              ? "Lock is ON below — this assignment is authoritative and members can't override it."
              : "Lock is OFF below — this only seeds new members, who may then change their own lanes."}
          </p>
        </div>
        {dirty && (
          <Button
            variant="outline"
            size="sm"
            className="shrink-0"
            disabled={saving}
            onClick={save}
          >
            {saving ? "Saving…" : "Save default"}
          </Button>
        )}
      </div>

      {connectedModels.isError ? (
        <InlineError
          message={
            connectedModels.error instanceof Error
              ? connectedModels.error.message
              : "Failed to load models"
          }
          onRetry={() => void connectedModels.refetch()}
        />
      ) : connectedModels.isLoading ? (
        <div className="py-6 text-center text-sm text-muted-foreground">Loading models…</div>
      ) : (
        <div className="flex flex-col gap-4">
          {(connectedModels.data?.length ?? 0) === 0 && (
            <p className="rounded-lg border border-dashed border-border bg-card/50 px-4 py-3 text-xs text-muted-foreground">
              Connect a provider on the Connections tab to pick org-default models.
              You can still leave a lane empty — it falls back to the deployment default.
            </p>
          )}
          {MODEL_LANE_KEYS.map((lane) => (
            <OrgLaneEditor
              key={lane}
              lane={lane}
              order={lanes[lane]}
              modelsById={modelsById}
              availableToAdd={(connectedModels.data ?? []).filter(
                (model) => !lanes[lane].includes(model.id),
              )}
              onAdd={(model) => addModel(lane, model)}
              onRemove={(id) => removeModel(lane, id)}
              onMove={(index, dir) => move(lane, index, dir)}
            />
          ))}
        </div>
      )}
    </section>
  );
}

function OrgLaneEditor({
  lane,
  order,
  modelsById,
  availableToAdd,
  onAdd,
  onRemove,
  onMove,
}: {
  lane: ModelLaneKey;
  order: string[];
  modelsById: Map<string, UserModel>;
  availableToAdd: UserModel[];
  onAdd: (model: UserModel) => void;
  onRemove: (id: string) => void;
  onMove: (index: number, dir: -1 | 1) => void;
}) {
  const meta = LANE_META[lane];
  return (
    <div className="flex flex-col gap-2 rounded-lg border bg-card/40 p-4">
      <div className="flex items-start justify-between gap-4">
        <div>
          <h4 className="text-sm font-semibold text-foreground">{meta.title}</h4>
          <p className="text-xs text-muted-foreground/70">{meta.roles}</p>
        </div>
        <ConnectedModelPicker models={availableToAdd} onSelect={onAdd} />
      </div>
      {order.length === 0 ? (
        <div className="rounded-md border border-dashed p-4 text-center text-xs text-muted-foreground">
          No org-default for this role. Add one — or it falls back to the deployment default.
        </div>
      ) : (
        <ul className="flex flex-col gap-2">
          {order.map((modelId, index) => {
            const model = modelsById.get(modelId);
            const providerId = model?.provider_id ?? modelId.split("/")[0] ?? "unknown";
            const name = model?.name ?? stripProviderPrefix(modelId);
            return (
              <li
                key={modelId}
                className="flex items-center gap-3 rounded-lg border bg-card px-4 py-3"
              >
                <div className="flex shrink-0 flex-col">
                  <button
                    type="button"
                    aria-label="Move up"
                    disabled={index === 0}
                    onClick={() => onMove(index, -1)}
                    className={cn(
                      "px-1 text-xs text-muted-foreground hover:text-foreground",
                      index === 0 && "opacity-30",
                    )}
                  >
                    ▲
                  </button>
                  <button
                    type="button"
                    aria-label="Move down"
                    disabled={index === order.length - 1}
                    onClick={() => onMove(index, 1)}
                    className={cn(
                      "px-1 text-xs text-muted-foreground hover:text-foreground",
                      index === order.length - 1 && "opacity-30",
                    )}
                  >
                    ▼
                  </button>
                </div>
                <div className="min-w-0 flex-1">
                  <div className="truncate font-semibold">{name}</div>
                  <div className="text-xs text-muted-foreground/60">{formatProvider(providerId)}</div>
                </div>
                <Button
                  variant="ghost"
                  size="sm"
                  aria-label={`Remove ${name}`}
                  onClick={() => onRemove(modelId)}
                  className="h-8 w-8 shrink-0 p-0 text-destructive hover:bg-destructive/10 hover:text-destructive"
                >
                  <HugeiconsIcon icon={Delete02Icon} size={16} />
                </Button>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
