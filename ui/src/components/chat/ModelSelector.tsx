import { useState } from 'react';
import {
  ModelSelector as ModelSelectorRoot,
  ModelSelectorContent,
  ModelSelectorEmpty,
  ModelSelectorGroup,
  ModelSelectorInput,
  ModelSelectorItem,
  ModelSelectorList,
  ModelSelectorLogo,
  ModelSelectorName,
  ModelSelectorTrigger,
} from '@/components/ai-elements/model-selector';
import { Tick02Icon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';

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

  const displayName =
    selectedModel !== 'unknown/model'
      ? modelNameById.get(selectedModel) ?? selectedModel
      : 'Select model';

  return (
    <ModelSelectorRoot open={open} onOpenChange={setOpen}>
      <ModelSelectorTrigger
        className="flex h-7 items-center gap-1.5 rounded-lg px-2 text-xs text-muted-foreground transition-colors hover:text-foreground"
      >
        {selectedModel !== 'unknown/model' && (
          <ModelSelectorLogo
            provider={groupedModels.find((g) => g.models.some((m) => m.id === selectedModel))?.providerId ?? ''}
          />
        )}
        <span className="truncate max-w-40">{displayName}</span>
      </ModelSelectorTrigger>

      <ModelSelectorContent title="Select a model">
        <ModelSelectorInput placeholder="Search models…" />
        <ModelSelectorList>
          <ModelSelectorEmpty>No models found.</ModelSelectorEmpty>
          {groupedModels.map((group) => (
            <ModelSelectorGroup key={group.providerId} heading={group.providerLabel}>
              {group.models.map((model) => (
                <ModelSelectorItem
                  key={model.id}
                  searchValue={model.name}
                  onSelect={() => {
                    onModelChange(model.id);
                    setOpen(false);
                  }}
                >
                  <ModelSelectorLogo provider={group.providerId} />
                  <ModelSelectorName>{model.name}</ModelSelectorName>
                  {model.id === selectedModel && (
                    <HugeiconsIcon icon={Tick02Icon} size={14} className="shrink-0 text-primary" />
                  )}
                </ModelSelectorItem>
              ))}
            </ModelSelectorGroup>
          ))}
        </ModelSelectorList>
      </ModelSelectorContent>
    </ModelSelectorRoot>
  );
}
