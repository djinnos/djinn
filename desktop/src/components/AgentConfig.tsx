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
  max_concurrent: number;
}

function ModelPicker({
  availableModels,
  onSelect,
}: {
  availableModels: ProviderModel[];
  onSelect: (model: ProviderModel) => void;
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
    <Combobox items={groups} value={value} onValueChange={handleValueChange}>
      <ComboboxInput placeholder="Search models..." showClear={false} className="w-full" />
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
                    <ComboboxItem key={`${provId}/${item.id}`} value={`${provId}/${item.id}`}>
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

  return (
    <div className="space-y-4">
      {error && (
        <div className="rounded-md bg-destructive/10 p-4 text-sm text-destructive flex items-center justify-between">
          <span>{error}</span>
          <Button variant="ghost" size="sm" onClick={onDismissError}>Dismiss</Button>
        </div>
      )}

      <div className="flex items-center justify-between">
        <div>
          <h3 className="text-lg font-semibold">Models</h3>
          <p className="text-sm text-muted-foreground">Add models and drag to reorder priority.</p>
        </div>
        {hasUnsavedChanges && (
          <Button onClick={onSave} disabled={isSaving} size="sm">
            {isSaving ? "Saving..." : "Save Changes"}
          </Button>
        )}
      </div>

      {isLoading ? (
        <div className="py-8 text-center text-sm text-muted-foreground">Loading...</div>
      ) : (
        <>
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
              {models.map((entry, index) => (
                <div
                  key={`${entry.provider}-${entry.model}-${index}`}
                  draggable
                  onDragStart={() => setDraggedItem(index)}
                  onDragOver={(e) => handleDragOver(e, index)}
                  onDrop={(e) => handleDrop(e, index)}
                  onDragEnd={() => { setDraggedItem(null); setDragOverIndex(null); }}
                  className={cn(
                    "transition-all",
                    dragOverIndex === index && "border-t-2 border-primary pt-1",
                    draggedItem === index && "opacity-50",
                  )}
                >
                  <div className="flex items-center gap-3 rounded-md border bg-card p-3">
                    <div className="cursor-grab text-muted-foreground shrink-0">
                      <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                        <circle cx="9" cy="5" r="1" /><circle cx="9" cy="12" r="1" /><circle cx="9" cy="19" r="1" />
                        <circle cx="15" cy="5" r="1" /><circle cx="15" cy="12" r="1" /><circle cx="15" cy="19" r="1" />
                      </svg>
                    </div>
                    <div className="min-w-0 flex-1">
                      <div className="font-medium truncate">{entry.model}</div>
                      <div className="text-xs text-muted-foreground">{entry.provider}</div>
                    </div>
                    <div className="flex items-center gap-2 shrink-0">
                      <Label className="text-xs text-muted-foreground">Max:</Label>
                      <Input
                        type="number"
                        min={1}
                        max={10}
                        value={entry.max_concurrent}
                        onChange={(e) => {
                          const v = parseInt(e.target.value, 10);
                          if (!isNaN(v) && v >= 1 && v <= 10) onUpdateMaxSessions(index, v);
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
                      <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                        <path d="M18 6 6 18" /><path d="m6 6 12 12" />
                      </svg>
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </>
      )}
    </div>
  );
}
