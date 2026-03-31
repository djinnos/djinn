import { useEffect, useRef, useState } from 'react';
import {
  Command,
  CommandInput,
  CommandList,
  CommandEmpty,
  CommandGroup,
  CommandItem,
} from '@/components/ui/command';
import { ArrowDown01Icon, Tick02Icon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import { cn } from '@/lib/utils';

interface ModelGroup {
  providerId: string;
  providerLabel: string;
  models: { id: string; name: string }[];
}

interface ModelSelectorProps {
  selectedModel: string;
  modelNameById: Map<string, string>;
  groupedModels: ModelGroup[];
  onModelChange: (value: string | null) => void;
}

export function ModelSelector({
  selectedModel,
  modelNameById,
  groupedModels,
  onModelChange,
}: ModelSelectorProps) {
  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement | null>(null);

  const displayName =
    selectedModel !== 'unknown/model'
      ? modelNameById.get(selectedModel) ?? selectedModel
      : 'Select model';

  useEffect(() => {
    if (!open) return;

    const handleClickOutside = (event: MouseEvent) => {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) {
        setOpen(false);
      }
    };

    const handleEscape = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setOpen(false);
    };

    document.addEventListener('mousedown', handleClickOutside);
    document.addEventListener('keydown', handleEscape);
    return () => {
      document.removeEventListener('mousedown', handleClickOutside);
      document.removeEventListener('keydown', handleEscape);
    };
  }, [open]);

  return (
    <div ref={containerRef} className="relative">
      <button
        type="button"
        onClick={() => setOpen((prev) => !prev)}
        className="flex h-7 items-center gap-1 rounded-lg px-2 text-xs text-muted-foreground transition-colors hover:text-foreground"
      >
        <span className="truncate max-w-40">{displayName}</span>
        <HugeiconsIcon
          icon={ArrowDown01Icon}
          size={12}
          className={cn('shrink-0 transition-transform', open && 'rotate-180')}
        />
      </button>

      {open && (
        <div className="absolute bottom-full left-0 z-50 mb-2 w-64 overflow-hidden rounded-lg border border-border bg-popover shadow-lg">
          <Command>
            <CommandInput placeholder="Search models…" />
            <CommandList>
              <CommandEmpty>No models found.</CommandEmpty>
              {groupedModels.map((group) => (
                <CommandGroup key={group.providerId} heading={group.providerLabel}>
                  {group.models.map((model) => (
                    <CommandItem
                      key={model.id}
                      searchValue={model.name}
                      onSelect={() => {
                        onModelChange(model.id);
                        setOpen(false);
                      }}
                    >
                      <span className="flex-1 truncate">{model.name}</span>
                      {model.id === selectedModel && (
                        <HugeiconsIcon icon={Tick02Icon} size={14} className="shrink-0 text-primary" />
                      )}
                    </CommandItem>
                  ))}
                </CommandGroup>
              ))}
            </CommandList>
          </Command>
        </div>
      )}
    </div>
  );
}
