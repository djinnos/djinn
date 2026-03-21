import { useMemo, useState } from "react";
import { ProviderModel } from "@/api/settings";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";
import {
  Combobox,
  ComboboxCollection,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxGroup,
  ComboboxInput,
  ComboboxItem,
  ComboboxLabel,
  ComboboxList,
  ComboboxSeparator,
} from "@/components/ui/combobox";

export interface AgentModelEntry {
  model: string;
  provider: string;
  enabledRoles?: string[];
  maxConcurrent?: number;
  max_concurrent?: number;
}

const ROLES: { key: string; label: string; short: string }[] = [
  { key: "worker", label: "Worker", short: "W" },
  { key: "reviewer", label: "Reviewer", short: "R" },
  { key: "lead", label: "Lead", short: "L" },
];

function ModelPicker({
  availableModels,
  onSelect,
  placeholder = "Search models...",
}: {
  availableModels: ProviderModel[];
  onSelect: (model: ProviderModel) => void;
  placeholder?: string;
}) {
  const [value, setValue] = useState<string | null>(null);

  const groups = useMemo(() => {
    const map = new Map<string, ProviderModel[]>();
    for (const m of availableModels) {
      const provId = m.provider_id ?? m.provider ?? "unknown";
      if (!map.has(provId)) map.set(provId, []);
      map.get(provId)!.push(m);
    }
    return Array.from(map.entries())
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([provider, items]) => ({
        provider,
        items: items.slice().sort((a, b) => a.name.localeCompare(b.name)),
      }));
  }, [availableModels]);

  const handleValueChange = (val: string | null) => {
    if (!val) return;
    const model = availableModels.find((m) => {
      const provId = m.provider_id ?? m.provider ?? "unknown";
      return `${provId}/${m.id}` === val;
    });
    if (model) {
      onSelect(model);
      setTimeout(() => setValue(null), 0);
    }
  };

  return (
    <div className="w-full">
      <Combobox items={groups} value={value} onValueChange={handleValueChange}>
        <ComboboxInput placeholder={placeholder} showClear={false} className="w-full" />
        <ComboboxContent>
          <ComboboxEmpty>No models found.</ComboboxEmpty>
          <ComboboxList>
            {(group, index) => (
              <ComboboxGroup key={group.provider} items={group.items}>
                <ComboboxLabel>{group.provider}</ComboboxLabel>
                <ComboboxCollection>
                  {(item) => {
                    const provId = item.provider_id ?? item.provider ?? "unknown";
                    return (
                      <ComboboxItem
                        key={`${provId}/${item.id}`}
                        value={`${provId}/${item.id}`}
                      >
                        {item.name}
                      </ComboboxItem>
                    );
                  }}
                </ComboboxCollection>
                {index < groups.length - 1 && <ComboboxSeparator />}
              </ComboboxGroup>
            )}
          </ComboboxList>
        </ComboboxContent>
      </Combobox>
    </div>
  );
}

interface AgentConfigProps {
  models: AgentModelEntry[];
  availableModels: ProviderModel[];
  isLoading: boolean;
  isSaving: boolean;
  error: string | null;
  hasUnsavedChanges: boolean;
  onAddModel: (model: { model: string; provider: string }) => void;
  onRemoveModel: (index: number) => void;
  onReorderModels: (fromIndex: number, toIndex: number) => void;
  onToggleRole: (indexOrModelId: number | string, role: string) => void;
  onUpdateMaxSessions: (indexOrModelId: number | string, maxConcurrent: number) => void;
  memoryModel: string | null;
  onSetMemoryModel: (modelId: string) => void;
  onDismissError: () => void;
  onSave: () => void;
}

export function AgentConfig({
  models,
  availableModels,
  isLoading,
  isSaving,
  error,
  hasUnsavedChanges,
  onAddModel,
  onRemoveModel,
  onReorderModels,
  onToggleRole,
  onUpdateMaxSessions,
  memoryModel,
  onSetMemoryModel,
  onDismissError,
  onSave,
}: AgentConfigProps) {
  const [draggedItem, setDraggedItem] = useState<number | null>(null);
  const [dragOverIndex, setDragOverIndex] = useState<number | null>(null);

  const handleDragStart = (index: number) => setDraggedItem(index);

  const handleDragOver = (e: React.DragEvent, index: number) => {
    e.preventDefault();
    if (draggedItem === null || draggedItem === index) return;
    setDragOverIndex(index);
  };

  const handleDrop = (e: React.DragEvent, toIndex: number) => {
    e.preventDefault();
    if (draggedItem === null) return;
    onReorderModels(draggedItem, toIndex);
    setDraggedItem(null);
    setDragOverIndex(null);
  };

  const handleDragEnd = () => {
    setDraggedItem(null);
    setDragOverIndex(null);
  };

  return (
    <div className="space-y-4">
      {error && (
        <div className="rounded-md bg-destructive/10 p-4 text-sm text-destructive">
          <div className="flex items-center justify-between">
            <span>{error}</span>
            <Button variant="ghost" size="sm" onClick={onDismissError}>Dismiss</Button>
          </div>
        </div>
      )}

      {isLoading ? (
        <div className="flex items-center justify-center py-8 text-muted-foreground">
          Loading settings...
        </div>
      ) : (
        <>
          <div className="flex items-center justify-between">
            <div>
              <h3 className="text-lg font-semibold">Model Configuration</h3>
              <p className="text-sm text-muted-foreground">Add models and set max concurrent sessions. Drag to reorder priority.</p>
            </div>
          </div>

          {/* Role legend */}
          <div className="flex items-center gap-4 text-xs text-muted-foreground">
            {ROLES.map((r) => (
              <span key={r.key}>{r.short} = {r.label}</span>
            ))}
          </div>

          <ModelPicker
            availableModels={availableModels}
            onSelect={(m) => onAddModel({ model: m.id, provider: m.provider_id ?? m.provider ?? "unknown" })}
          />

          {models.length === 0 ? (
            <div className="rounded-md border border-dashed p-6 text-center text-sm text-muted-foreground">
              No models configured. Add models from connected providers above.
            </div>
          ) : (
            <div className="space-y-2">
              {models.map((entry, index) => {
                const maxConcurrent = entry.max_concurrent ?? entry.maxConcurrent ?? 1;
                const enabledRoles = entry.enabledRoles ?? [];
                return (
                  <div
                    key={`${entry.provider}-${entry.model}-${index}`}
                    draggable
                    onDragStart={() => handleDragStart(index)}
                    onDragOver={(e) => handleDragOver(e, index)}
                    onDrop={(e) => handleDrop(e, index)}
                    onDragEnd={handleDragEnd}
                    className={cn(
                      "transition-all",
                      dragOverIndex === index && "border-t-2 border-primary pt-1",
                      draggedItem === index ? "opacity-50" : "opacity-100",
                    )}
                  >
                    <div className="flex items-center gap-3 rounded-md border bg-card p-3">
                      <div className="flex flex-col text-muted-foreground cursor-grab shrink-0">
                        <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                          <circle cx="9" cy="12" r="1" /><circle cx="9" cy="5" r="1" /><circle cx="9" cy="19" r="1" />
                          <circle cx="15" cy="12" r="1" /><circle cx="15" cy="5" r="1" /><circle cx="15" cy="19" r="1" />
                        </svg>
                      </div>
                      <div className="min-w-0 flex-1">
                        <div className="font-medium truncate">{entry.model}</div>
                        <div className="text-xs text-muted-foreground">{entry.provider}</div>
                      </div>
                      {/* Role toggles */}
                      <div className="flex items-center gap-1 shrink-0">
                        {ROLES.map((r) => {
                          const isEnabled = enabledRoles.includes(r.key);
                          return (
                            <button
                              key={r.key}
                              type="button"
                              title={isEnabled ? `Disable for ${r.label}` : `Enable for ${r.label}`}
                              onClick={() => onToggleRole(index, r.key)}
                              className={cn(
                                "rounded px-1.5 py-0.5 text-xs font-medium transition-colors",
                                isEnabled
                                  ? "bg-primary text-primary-foreground"
                                  : "bg-muted text-muted-foreground hover:bg-muted/80",
                              )}
                            >
                              {r.short}
                            </button>
                          );
                        })}
                      </div>
                      <div className="flex items-center gap-2 shrink-0">
                        <Label className="text-xs text-muted-foreground whitespace-nowrap">Max:</Label>
                        <Input
                          type="number"
                          min={1}
                          max={10}
                          value={maxConcurrent}
                          onChange={(e) => {
                            const value = parseInt(e.target.value, 10);
                            if (!isNaN(value) && value >= 1 && value <= 10) {
                              onUpdateMaxSessions(index, value);
                            }
                          }}
                          className="w-16 h-8"
                        />
                      </div>
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() => onRemoveModel(index)}
                        className="h-8 w-8 p-0 shrink-0"
                      >
                        <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                          <path d="M18 6 6 18" /><path d="m6 6 12 12" />
                        </svg>
                      </Button>
                    </div>
                  </div>
                );
              })}
            </div>
          )}
          {/* Memory Model */}
          <div className="space-y-2">
            <Label className="text-sm font-medium">Memory Model</Label>
            <ModelPicker
              availableModels={availableModels}
              placeholder="Select memory model..."
              onSelect={(m) => onSetMemoryModel(m.id)}
            />
            {memoryModel && (
              <p className="text-xs text-muted-foreground">Current: {memoryModel}</p>
            )}
          </div>

          {/* Save button */}
          {(hasUnsavedChanges || isSaving) && (
            <Button onClick={onSave} disabled={isSaving} className="w-full">
              {isSaving ? "Saving..." : "Save Changes"}
            </Button>
          )}
        </>
      )}
    </div>
  );
}
