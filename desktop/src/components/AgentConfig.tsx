import { useMemo, useRef, useState } from "react";
import { AgentRole, ModelPriorityItem, ProviderModel } from "@/api/settings";
import { UnifiedModelEntry } from "@/stores/settingsStore";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";

const ROLE_LABELS: Record<AgentRole, string> = {
  worker: "W",
  reviewer: "R",
  lead: "L",
  planner: "P",
  architect: "A",
};

const ROLE_FULL_LABELS: Record<AgentRole, string> = {
  worker: "Worker",
  reviewer: "Reviewer",
  lead: "Lead",
  planner: "Planner",
  architect: "Architect",
};

const ALL_ROLES: AgentRole[] = ["worker", "reviewer", "lead", "planner", "architect"];

function ModelPicker({
  availableModels,
  onSelect,
  placeholder = "Search models...",
}: {
  availableModels: ProviderModel[];
  onSelect: (model: ProviderModel) => void;
  placeholder?: string;
}) {
  const [query, setQuery] = useState("");
  const [open, setOpen] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);

  const filtered = useMemo(() => {
    if (!query.trim()) return availableModels;
    const q = query.toLowerCase();
    return availableModels.filter(
      (m) =>
        m.name.toLowerCase().includes(q) ||
        m.provider_id.toLowerCase().includes(q) ||
        m.id.toLowerCase().includes(q),
    );
  }, [availableModels, query]);

  const handleSelect = (model: ProviderModel) => {
    onSelect(model);
    setQuery("");
    setOpen(false);
  };

  const handleBlur = (e: React.FocusEvent) => {
    // Don't close if focus moves within the container
    if (containerRef.current?.contains(e.relatedTarget)) return;
    setOpen(false);
  };

  return (
    <div ref={containerRef} className="relative flex gap-2" onBlur={handleBlur}>
      <Input
        ref={inputRef}
        placeholder={placeholder}
        value={query}
        onChange={(e) => {
          setQuery(e.target.value);
          setOpen(true);
        }}
        onFocus={() => setOpen(true)}
      />
      {open && (
        <div className="absolute top-full left-0 right-0 z-50 mt-1 max-h-64 overflow-y-auto rounded-lg border bg-popover shadow-md">
          {filtered.length === 0 ? (
            <div className="p-3 text-center text-sm text-muted-foreground">
              No models found.
            </div>
          ) : (
            filtered.map((m) => (
              <button
                key={`${m.provider_id}::${m.id}`}
                type="button"
                className="flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-accent hover:text-accent-foreground"
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => handleSelect(m)}
              >
                <span className="font-medium">{m.name}</span>
                <span className="text-xs text-muted-foreground">{m.provider_id}</span>
              </button>
            ))
          )}
        </div>
      )}
    </div>
  );
}

interface AgentConfigProps {
  models: UnifiedModelEntry[];
  availableModels: ProviderModel[];
  memoryModel: string | null;
  isLoading: boolean;
  isSaving: boolean;
  error: string | null;
  hasUnsavedChanges: boolean;
  onAddModel: (model: ModelPriorityItem) => void;
  onRemoveModel: (index: number) => void;
  onReorderModels: (fromIndex: number, toIndex: number) => void;
  onToggleRole: (index: number, role: AgentRole) => void;
  onUpdateMaxSessions: (index: number, maxConcurrent: number) => void;
  onSetMemoryModel: (modelId: string | null) => void;
  onDismissError: () => void;
  onSave: () => void;
}

export function AgentConfig({
  models,
  availableModels,
  memoryModel,
  isLoading,
  isSaving,
  error,
  hasUnsavedChanges,
  onAddModel,
  onRemoveModel,
  onReorderModels,
  onToggleRole,
  onUpdateMaxSessions,
  onSetMemoryModel,
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
                Add models, set session limits, and toggle which agents can use each model. Drag to reorder priority.
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

          {/* Role Legend */}
          <div className="flex items-center gap-4 text-xs text-muted-foreground">
            {ALL_ROLES.map((role) => (
              <span key={role}>
                <span className="font-semibold text-foreground">{ROLE_LABELS[role]}</span>
                {" = "}
                {ROLE_FULL_LABELS[role]}
              </span>
            ))}
          </div>

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

                    {/* Agent Role Toggles */}
                    <div className="flex items-center gap-1 shrink-0">
                      {ALL_ROLES.map((role) => {
                        const enabled = entry.enabledRoles.includes(role);
                        return (
                          <button
                            key={role}
                            type="button"
                            title={`${enabled ? "Disable" : "Enable"} for ${ROLE_FULL_LABELS[role]}`}
                            onClick={() => onToggleRole(index, role)}
                            className={cn(
                              "flex h-7 min-w-[28px] items-center justify-center rounded px-1.5 text-xs font-semibold transition-colors",
                              enabled
                                ? "bg-primary text-primary-foreground"
                                : "bg-muted text-muted-foreground hover:bg-muted/80",
                            )}
                          >
                            {ROLE_LABELS[role]}
                          </button>
                        );
                      })}
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

          {/* Memory Model */}
          <div className="border-t pt-6 space-y-3">
            <div>
              <h3 className="text-lg font-semibold">Memory Model</h3>
              <p className="text-sm text-muted-foreground">
                Model used for knowledge extraction and summarisation after sessions complete.
                Defaults to the first agent model above if not set.
              </p>
            </div>

            {memoryModel ? (
              <div className="flex items-center gap-3 rounded-md border bg-card p-3">
                <div className="min-w-0 flex-1">
                  <div className="font-medium truncate">
                    {memoryModel.includes("/") ? memoryModel.split("/").slice(1).join("/") : memoryModel}
                  </div>
                  <div className="text-xs text-muted-foreground">
                    {memoryModel.includes("/") ? memoryModel.split("/")[0] : "unknown"}
                  </div>
                </div>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => onSetMemoryModel(null)}
                  className="h-8 shrink-0 text-xs text-muted-foreground"
                >
                  Clear
                </Button>
              </div>
            ) : (
              <ModelPicker
                availableModels={availableModels}
                onSelect={(m) => onSetMemoryModel(m.id)}
                placeholder="Select memory model..."
              />
            )}
          </div>
        </div>
      )}
    </div>
  );
}
