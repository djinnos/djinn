import { useMemo, useState } from "react";
import { AgentRole, ModelPriorityItem, ProviderModel, ModelSessionLimit } from "@/api/settings";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Combobox,
  ComboboxInput,
  ComboboxContent,
  ComboboxList,
  ComboboxItem,
  ComboboxEmpty,
  ComboboxCollection,
} from "@/components/ui/combobox";
import { Separator } from "@/components/ui/separator";
import { Badge } from "@/components/ui/badge";

const ROLE_LABELS: Record<AgentRole, string> = {
  worker: "Worker",
  task_reviewer: "Task Reviewer",
  conflict_resolver: "Conflict Resolver",
  pm: "PM",
};

interface DraggableItemProps {
  item: ModelPriorityItem;
  index: number;
  onRemove: (index: number) => void;
}

function DraggableModelItem({ item, index, onRemove }: DraggableItemProps) {
  return (
    <div
      className="flex items-center justify-between gap-2 rounded-md border bg-card p-3 shadow-sm"
      draggable
    >
      <div className="flex items-center gap-3">
        <div className="flex flex-col text-muted-foreground">
          <svg
            xmlns="http://www.w3.org/2000/svg"
            width="16"
            height="16"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <circle cx="9" cy="12" r="1" />
            <circle cx="9" cy="5" r="1" />
            <circle cx="9" cy="19" r="1" />
            <circle cx="15" cy="12" r="1" />
            <circle cx="15" cy="5" r="1" />
            <circle cx="15" cy="19" r="1" />
          </svg>
        </div>
        <div>
          <div className="font-medium">{item.model}</div>
          <div className="text-xs text-muted-foreground">{item.provider}</div>
        </div>
      </div>
      <Button
        variant="ghost"
        size="sm"
        onClick={() => onRemove(index)}
        className="h-8 w-8 p-0"
      >
        <svg
          xmlns="http://www.w3.org/2000/svg"
          width="16"
          height="16"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          <path d="M18 6 6 18" />
          <path d="m6 6 12 12" />
        </svg>
      </Button>
    </div>
  );
}

interface ModelPrioritySectionProps {
  role: AgentRole;
  models: ModelPriorityItem[];
  availableModels: ProviderModel[];
  onAddModel: (role: AgentRole, model: ModelPriorityItem) => void;
  onRemoveModel: (role: AgentRole, index: number) => void;
  onReorder: (role: AgentRole, fromIndex: number, toIndex: number) => void;
}

function ModelPrioritySection({
  role,
  models,
  availableModels,
  onAddModel,
  onRemoveModel,
  onReorder,
}: ModelPrioritySectionProps) {
  const [selectedModel, setSelectedModel] = useState<string | null>(null);
  const [draggedItem, setDraggedItem] = useState<number | null>(null);
  const [dragOverIndex, setDragOverIndex] = useState<number | null>(null);

  const modelOptionKeys = useMemo(
    () => availableModels.map((m) => `${m.provider}::${m.id}`),
    [availableModels],
  );

  const handleAddModel = () => {
    if (!selectedModel) return;

    const [provider, modelId] = selectedModel.split("::");
    const model = availableModels.find(
      (m) => m.provider === provider && m.id === modelId
    );

    if (model) {
      onAddModel(role, { model: model.name, provider: model.provider });
      setSelectedModel(null);
    }
  };

  const handleDragStart = (index: number) => {
    setDraggedItem(index);
  };

  const handleDragOver = (e: React.DragEvent, index: number) => {
    e.preventDefault();
    if (draggedItem === null || draggedItem === index) return;
    setDragOverIndex(index);
  };

  const handleDrop = (e: React.DragEvent, toIndex: number) => {
    e.preventDefault();
    if (draggedItem === null) return;
    
    onReorder(role, draggedItem, toIndex);
    setDraggedItem(null);
    setDragOverIndex(null);
  };

  const handleDragEnd = () => {
    setDraggedItem(null);
    setDragOverIndex(null);
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{ROLE_LABELS[role]}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        {/* Add Model Section */}
        <div className="flex gap-2">
          <div className="flex-1">
          <Combobox
            value={selectedModel}
            onValueChange={setSelectedModel}
            items={modelOptionKeys}
            itemToStringLabel={(value) => {
              const [p, id] = (value ?? "").split("::");
              const m = availableModels.find((m) => m.provider === p && m.id === id);
              return m ? `${m.name} (${m.provider})` : value ?? "";
            }}
            filter={(value, query) => {
              const [p, id] = (value ?? "").split("::");
              const m = availableModels.find((m) => m.provider === p && m.id === id);
              const label = m ? `${m.name} ${m.provider}` : value ?? "";
              return label.toLowerCase().includes(query.toLowerCase());
            }}
          >
            <ComboboxInput placeholder="Select a model..." showClear={!!selectedModel} />
            <ComboboxContent>
              <ComboboxList>
                <ComboboxEmpty>No models found.</ComboboxEmpty>
                <ComboboxCollection>
                  {(value: string) => {
                    const [p, id] = value.split("::");
                    const m = availableModels.find((m) => m.provider === p && m.id === id);
                    return (
                      <ComboboxItem key={value} value={value}>
                        <div>
                          <span>{m?.name ?? value}</span>
                          <span className="ml-1.5 text-xs text-muted-foreground">{p}</span>
                        </div>
                      </ComboboxItem>
                    );
                  }}
                </ComboboxCollection>
              </ComboboxList>
            </ComboboxContent>
          </Combobox>
          </div>
          <Button onClick={handleAddModel} disabled={!selectedModel}>
            Add
          </Button>
        </div>

        {/* Model Priority List */}
        <div className="space-y-2">
          {models.length === 0 ? (
            <div className="rounded-md border border-dashed p-4 text-center text-sm text-muted-foreground">
              No models configured. Add models from connected providers.
            </div>
          ) : (
            models.map((item, index) => (
              <div
                key={`${item.provider}-${item.model}-${index}`}
                draggable
                onDragStart={() => handleDragStart(index)}
                onDragOver={(e) => handleDragOver(e, index)}
                onDrop={(e) => handleDrop(e, index)}
                onDragEnd={handleDragEnd}
                className={`
                  transition-all
                  ${dragOverIndex === index ? "border-t-2 border-primary pt-2" : ""}
                  ${draggedItem === index ? "opacity-50" : "opacity-100"}
                `}
              >
                <DraggableModelItem
                  item={item}
                  index={index}
                  onRemove={() => onRemoveModel(role, index)}
                />
              </div>
            ))
          )}
        </div>
      </CardContent>
    </Card>
  );
}

interface SessionLimitItemProps {
  model: string;
  provider: string;
  maxConcurrent: number;
  currentActive: number;
  onChange: (maxConcurrent: number) => void;
}

function SessionLimitItem({
  model,
  provider,
  maxConcurrent,
  currentActive,
  onChange,
}: SessionLimitItemProps) {
  return (
    <div className="flex items-center justify-between gap-4 rounded-md border bg-card p-3">
      <div className="flex-1">
        <div className="font-medium">{model}</div>
        <div className="text-xs text-muted-foreground">{provider}</div>
      </div>
      <div className="flex items-center gap-4">
        <div className="flex items-center gap-2">
          <span className="text-sm text-muted-foreground">Active:</span>
          <Badge variant="secondary">{currentActive}</Badge>
        </div>
        <div className="flex items-center gap-2">
          <Label htmlFor={`session-limit-${model}-${provider}`} className="text-sm">
            Max:
          </Label>
          <Input
            id={`session-limit-${model}-${provider}`}
            type="number"
            min={1}
            max={10}
            value={maxConcurrent}
            onChange={(e) => {
              const value = parseInt(e.target.value, 10);
              if (!isNaN(value) && value >= 1 && value <= 10) {
                onChange(value);
              }
            }}
            className="w-20"
          />
        </div>
      </div>
    </div>
  );
}

interface AgentConfigProps {
  modelPriorities: Record<AgentRole, ModelPriorityItem[]>;
  sessionLimits: ModelSessionLimit[];
  availableModels: ProviderModel[];
  isLoading: boolean;
  isSaving: boolean;
  error: string | null;
  hasUnsavedChanges: boolean;
  onAddModel: (role: AgentRole, model: ModelPriorityItem) => void;
  onRemoveModel: (role: AgentRole, index: number) => void;
  onReorderModels: (role: AgentRole, fromIndex: number, toIndex: number) => void;
  onUpdateSessionLimit: (model: string, provider: string, maxConcurrent: number) => void;
  onDismissError: () => void;
  onSave: () => void;
}

export function AgentConfig({
  modelPriorities,
  sessionLimits,
  availableModels,
  isLoading,
  isSaving,
  error,
  hasUnsavedChanges,
  onAddModel,
  onRemoveModel,
  onReorderModels,
  onUpdateSessionLimit,
  onDismissError,
  onSave,
}: AgentConfigProps) {

  const roles: AgentRole[] = ["worker", "task_reviewer", "conflict_resolver", "pm"];

  // Get all models that are in use across all roles
  const modelsInUse = new Set<string>();
  roles.forEach((role) => {
    modelPriorities[role].forEach((item) => {
      modelsInUse.add(`${item.provider}::${item.model}`);
    });
  });

  // Build session limits for models in use
  const sessionLimitItems = Array.from(modelsInUse).map((key) => {
    const [provider, modelName] = key.split("::");
    const existingLimit = sessionLimits.find(
      (sl) => sl.model === modelName && sl.provider === provider
    );
    return {
      model: modelName,
      provider,
      max_concurrent: existingLimit?.max_concurrent ?? 1,
      current_active: existingLimit?.current_active ?? 0,
    };
  });

  return (
    <div className="space-y-6">
      {error && (
        <div className="rounded-md bg-destructive/10 p-4 text-sm text-destructive">
          <div className="flex items-center justify-between">
            <span>{error}</span>
            <Button variant="ghost" size="sm" onClick={onDismissError}>
              Dismiss
            </Button>
          </div>
        </div>
      )}

      {isLoading ? (
        <div className="flex items-center justify-center py-8">
          <div className="text-muted-foreground">Loading settings...</div>
        </div>
      ) : (
        <>
          {/* Model Priority Configuration */}
          <div className="space-y-4">
            <div className="flex items-center justify-between">
              <div>
                <h3 className="text-lg font-semibold">Model Priority Lists</h3>
                <p className="text-sm text-muted-foreground">
                  Configure which models to use for each agent role. Drag to reorder.
                </p>
              </div>
              {hasUnsavedChanges && (
                <Button onClick={onSave} disabled={isSaving} size="sm">
                  {isSaving ? "Saving..." : "Save Changes"}
                </Button>
              )}
            </div>

            <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
              {roles.map((role) => (
                <ModelPrioritySection
                  key={role}
                  role={role}
                  models={modelPriorities[role]}
                  availableModels={availableModels}
                  onAddModel={onAddModel}
                  onRemoveModel={onRemoveModel}
                  onReorder={onReorderModels}
                />
              ))}
            </div>
          </div>

          <Separator />

          {/* Session Limits */}
          <div className="space-y-4">
            <div>
              <h3 className="text-lg font-semibold">Session Limits</h3>
              <p className="text-sm text-muted-foreground">
                Configure maximum concurrent sessions for each model.
              </p>
            </div>

            {sessionLimitItems.length === 0 ? (
              <div className="rounded-md border border-dashed p-4 text-center text-sm text-muted-foreground">
                No models in use. Add models to the priority lists above to configure session limits.
              </div>
            ) : (
              <div className="space-y-2">
                {sessionLimitItems.map((item) => (
                  <SessionLimitItem
                    key={`${item.provider}-${item.model}`}
                    model={item.model}
                    provider={item.provider}
                    maxConcurrent={item.max_concurrent}
                    currentActive={item.current_active}
                    onChange={(max) =>
                      onUpdateSessionLimit(item.model, item.provider, max)
                    }
                  />
                ))}
              </div>
            )}
          </div>
        </>
      )}
    </div>
  );
}
