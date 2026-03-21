import { useMemo, useState } from "react";
import { Delete02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { ProviderModel } from "@/api/settings";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";

/** Maps known provider IDs to display names. Falls back to title-casing the id. */
function formatProvider(id: string): string {
  const known: Record<string, string> = {
    openai: "OpenAI",
    anthropic: "Anthropic",
    google: "Google",
    azure: "Azure",
    aws: "AWS",
    mistral: "Mistral",
    cohere: "Cohere",
    groq: "Groq",
    ollama: "Ollama",
    lmstudio: "LM Studio",
    moonshot: "Moonshot",
    deepseek: "DeepSeek",
    perplexity: "Perplexity",
  };
  return known[id.toLowerCase()] ?? id.replace(/-/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());
}
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
      <ComboboxInput placeholder="Search or add models..." showClear={false} className="w-full" />
      <ComboboxContent>
        <ComboboxEmpty>No models found.</ComboboxEmpty>
        <ComboboxList>
          {(group, index) => (
            <ComboboxGroup key={group.provider} items={group.items}>
              <ComboboxLabel>{formatProvider(group.provider)}</ComboboxLabel>
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

      <div className="flex items-start justify-between gap-4">
        <div>
          <h3 className="text-xl font-bold">Models</h3>
          <p className="text-sm text-muted-foreground">Priority = top → bottom (fallback order)</p>
        </div>
        <div className="flex items-center gap-2 shrink-0">
          {hasUnsavedChanges && (
            <Button variant="outline" onClick={onSave} disabled={isSaving} size="sm">
              {isSaving ? "Saving..." : "Save"}
            </Button>
          )}
        </div>
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
            <div className="rounded-md border border-dashed p-8 text-center text-sm text-muted-foreground">
              No models configured. Search above to add models.
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
                  <div className="flex items-center gap-3 rounded-lg border bg-card px-4 py-3">
                    {/* Drag handle */}
                    <div className="cursor-grab text-muted-foreground/40 shrink-0 select-none">
                      <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
                        <circle cx="9" cy="5" r="1.5" /><circle cx="9" cy="12" r="1.5" /><circle cx="9" cy="19" r="1.5" />
                        <circle cx="15" cy="5" r="1.5" /><circle cx="15" cy="12" r="1.5" /><circle cx="15" cy="19" r="1.5" />
                      </svg>
                    </div>
                    {/* Model info */}
                    <div className="min-w-0 flex-1">
                      <div className="font-semibold truncate">{entry.model}</div>
                      <div className="text-xs text-muted-foreground/60 uppercase tracking-wide">{formatProvider(entry.provider)}</div>
                    </div>
                    {/* Max sessions */}
                    <div className="flex items-center gap-2 shrink-0">
                      <Label className="text-sm text-muted-foreground">Max:</Label>
                      <Input
                        type="number"
                        min={1}
                        max={10}
                        value={entry.max_concurrent}
                        onChange={(e) => {
                          const v = parseInt(e.target.value, 10);
                          if (!isNaN(v) && v >= 1 && v <= 10) onUpdateMaxSessions(index, v);
                        }}
                        className="w-16 h-9 text-center"
                      />
                    </div>
                    {/* Remove */}
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => onRemoveModel(index)}
                      className="shrink-0 text-destructive hover:text-destructive hover:bg-destructive/10 h-8 w-8 p-0"
                    >
                      <HugeiconsIcon icon={Delete02Icon} size={16} />
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
