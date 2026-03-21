import { useMemo, useState } from "react";
import { ProviderModel } from "@/api/settings";
import { ModelEntry } from "@/stores/settingsStore";
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
      if (!map.has(m.provider_id)) map.set(m.provider_id, []);
      map.get(m.provider_id)!.push(m);
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
    const model = availableModels.find((m) => `${m.provider_id}/${m.id}` === val);
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
                {(item) => (
                  <ComboboxItem
                    key={`${item.provider_id}/${item.id}`}
                    value={`${item.provider_id}/${item.id}`}
                  >
                    {item.name}
                  </ComboboxItem>
                )}
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
  models: ModelEntry[];
  availableModels: ProviderModel[];
  isLoading: boolean;
  isSaving: boolean;
  error: string | null;
  hasUnsavedChanges: boolean;
  onAddModel: (model: { model: string; provider: string }) => void;
  onRemoveModel: (index: number) => void;
  onReorderModels: (fromIndex: number, toIndex: number) => void;
  onUpdateMaxSessions: (index: number, maxConcurrent: number) => void;
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
  onUpdateMaxSessions,
  onDismissError,
  onSave,
}: AgentConfigProps) {
  const [draggedItem, setDraggedItem] = useState<number | null>(null);
  const [dragOverIndex, setDragOverIndex] = useState<number | null>(null);

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
    onReorderModels(draggedItem, toIndex);
    setDraggedItem(null);
    setDragOverIndex(null);
  };

  const handleDragEnd = () => {
    setDraggedItem(null);
    setDragOverIndex(null);
  };

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
        <div className="space-y-4">
          <div className="flex items-center justify-between">
            <div>
              <h3 className="text-lg font-semibold">Model Configuration</h3>
              <p className="text-sm text-muted-foreground">
                Add models and set max concurrent sessions. Drag to reorder priority.
              </p>
            </div>
            {hasUnsavedChanges && (
              <Button onClick={onSave} disabled={isSaving} size="sm">
                {isSaving ? "Saving..." : "Save Changes"}
              </Button>
            )}
          </div>

          {/* Add Model */}
          <ModelPicker
            availableModels={availableModels}
            onSelect={(m) => onAddModel({ model: m.id, provider: m.provider_id })}
          />

          {/* Model List */}
          {models.length === 0 ? (
            <div className="rounded-md border border-dashed p-6 text-center text-sm text-muted-foreground">
              No models configured. Add models from connected providers above.
            </div>
          ) : (
            <div className="space-y-2">
              {models.map((entry, index) => (
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
                    {/* Drag Handle */}
                    <div className="flex flex-col text-muted-foreground cursor-grab shrink-0">
                      <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                        <circle cx="9" cy="12" r="1" />
                        <circle cx="9" cy="5" r="1" />
                        <circle cx="9" cy="19" r="1" />
                        <circle cx="15" cy="12" r="1" />
                        <circle cx="15" cy="5" r="1" />
                        <circle cx="15" cy="19" r="1" />
                      </svg>
                    </div>

                    {/* Model Info */}
                    <div className="min-w-0 flex-1">
                      <div className="font-medium truncate">{entry.model}</div>
                      <div className="text-xs text-muted-foreground">{entry.provider}</div>
                    </div>

                    {/* Max Sessions */}
                    <div className="flex items-center gap-2 shrink-0">
                      <Label className="text-xs text-muted-foreground whitespace-nowrap">Max:</Label>
                      <Input
                        type="number"
                        min={1}
                        max={10}
                        value={entry.max_concurrent}
                        onChange={(e) => {
                          const value = parseInt(e.target.value, 10);
                          if (!isNaN(value) && value >= 1 && value <= 10) {
                            onUpdateMaxSessions(index, value);
                          }
                        }}
                        className="w-16 h-8"
                      />
                    </div>

                    {/* Remove */}
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => onRemoveModel(index)}
                      className="h-8 w-8 p-0 shrink-0"
                    >
                      <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                        <path d="M18 6 6 18" />
                        <path d="m6 6 12 12" />
                      </svg>
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
