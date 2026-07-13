import { useMemo, useState, type ComponentProps, type ReactNode } from "react";
import { Tick02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import type { UserModel } from "@/api/userConfig";
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
import { Button } from "@/components/ui/button";

import {
  formatModelMetadata,
  groupModelsByProvider,
  stripProviderPrefix,
} from "./modelPicker";
import { formatProvider } from "./providerDisplay";

type PickerButtonProps = ComponentProps<typeof Button>;

interface ConnectedModelPickerProps {
  /** Models shown before the user searches or expands a provider. */
  models: UserModel[];
  /** Full connected catalog searched across every provider. */
  allModels?: UserModel[];
  onSelect: (model: UserModel) => void;
  triggerLabel?: ReactNode;
  triggerAriaLabel?: string;
  triggerClassName?: string;
  triggerVariant?: PickerButtonProps["variant"];
  triggerSize?: PickerButtonProps["size"];
  title?: string;
  emptyMessage?: string;
  disabled?: boolean;
  selectedModelId?: string;
}

/**
 * Shared connected-model picker used by Settings and first-run onboarding.
 * It keeps the initial list curated, searches the full catalog, and delegates
 * the bounded scroll region to ModelSelector.
 */
export function ConnectedModelPicker({
  models,
  allModels,
  onSelect,
  triggerLabel = "Add model",
  triggerAriaLabel,
  triggerClassName,
  triggerVariant = "default",
  triggerSize = "sm",
  title = "Add a model",
  emptyMessage = "No connected models available.",
  disabled = false,
  selectedModelId,
}: ConnectedModelPickerProps) {
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");
  const [expandedProviders, setExpandedProviders] = useState<Set<string>>(
    () => new Set(),
  );

  const defaultModelIds = useMemo(
    () => new Set(models.map((model) => model.id)),
    [models],
  );
  const searchableModels = allModels ?? models;
  const normalizedSearch = search.trim().toLowerCase();

  const groups = useMemo(() => {
    const source = normalizedSearch
      ? searchableModels.filter((model) =>
          modelMatchesSearch(model, normalizedSearch),
        )
      : searchableModels;

    return groupModelsByProvider(source)
      .map((group) => {
        const hiddenCount = group.models.filter(
          (model) => !defaultModelIds.has(model.id),
        ).length;
        const expanded = expandedProviders.has(group.providerId);
        const items =
          normalizedSearch || expanded
            ? group.models
            : group.models.filter((model) => defaultModelIds.has(model.id));
        return { ...group, hiddenCount, items };
      })
      .filter((group) => group.items.length > 0 || group.hiddenCount > 0);
  }, [
    defaultModelIds,
    expandedProviders,
    normalizedSearch,
    searchableModels,
  ]);

  const resetPicker = () => {
    setSearch("");
    setExpandedProviders(new Set());
  };

  return (
    <ModelSelector
      open={open}
      onOpenChange={(nextOpen) => {
        setOpen(nextOpen);
        if (!nextOpen) resetPicker();
      }}
    >
      <ModelSelectorTrigger
        disabled={disabled}
        render={
          <Button
            type="button"
            variant={triggerVariant}
            size={triggerSize}
            className={triggerClassName}
            aria-label={triggerAriaLabel}
          />
        }
      >
        {triggerLabel}
      </ModelSelectorTrigger>
      <ModelSelectorContent title={title} className="overflow-hidden sm:max-w-xl">
        <ModelSelectorInput
          placeholder="Search models…"
          aria-label={`Search ${title.toLowerCase()}`}
          onChange={(event) => setSearch(event.currentTarget.value)}
        />
        <ModelSelectorList>
          {groups.length === 0 && (
            <ModelSelectorEmpty>{emptyMessage}</ModelSelectorEmpty>
          )}
          {groups.map((group, index) => (
            <ModelSelectorGroup
              key={group.providerId}
              heading={formatProvider(group.providerId)}
            >
              {group.items.map((model) => {
                const name = model.name || stripProviderPrefix(model.id);
                const metadata = formatModelMetadata(model);

                return (
                  <ModelSelectorItem
                    key={model.id}
                    className="group cursor-pointer items-start gap-3 rounded-lg px-3 py-2.5 text-left transition-colors hover:bg-primary/10 hover:text-foreground focus-visible:bg-primary/10 focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-primary/50 aria-[current=true]:bg-primary/10 aria-[current=true]:text-foreground aria-[current=true]:ring-1 aria-[current=true]:ring-inset aria-[current=true]:ring-primary/30"
                    searchValue={
                      search.length > 0 ? search : modelSearchValue(model)
                    }
                    aria-current={
                      selectedModelId === model.id ? "true" : undefined
                    }
                    onSelect={() => {
                      onSelect(model);
                      setOpen(false);
                      resetPicker();
                    }}
                  >
                    <ModelSelectorLogo
                      provider={group.providerId}
                      className="mt-0.5 size-4 shrink-0"
                    />
                    <span className="min-w-0 flex-1 text-left">
                      <span className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
                        <ModelSelectorName
                          data-slot="model-picker-name"
                          className="min-w-0 overflow-visible text-clip whitespace-normal break-words font-medium leading-5 text-foreground"
                        >
                          {name}
                        </ModelSelectorName>
                        {model.recommended && (
                          <span className="shrink-0 rounded-full border border-primary/20 bg-primary/10 px-2 py-0.5 text-[11px] font-medium leading-4 text-primary">
                            Recommended
                          </span>
                        )}
                      </span>
                      {metadata && (
                        <span
                          data-slot="model-picker-metadata"
                          className="mt-0.5 block whitespace-normal break-words text-left text-xs leading-4 text-muted-foreground group-hover:text-foreground/75 group-focus-visible:text-foreground/75 group-aria-[current=true]:text-foreground/75"
                          title={metadata}
                        >
                          {metadata}
                        </span>
                      )}
                    </span>
                    {selectedModelId === model.id && (
                      <span className="mt-0.5 shrink-0 text-primary">
                        <HugeiconsIcon icon={Tick02Icon} size={16} />
                        <span className="sr-only">Selected</span>
                      </span>
                    )}
                  </ModelSelectorItem>
                );
              })}
              {!normalizedSearch &&
                !expandedProviders.has(group.providerId) &&
                group.hiddenCount > 0 && (
                  <button
                    type="button"
                    className="w-full rounded-md px-3 py-2 text-left text-xs font-medium text-primary hover:bg-primary/10 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-primary/50"
                    onClick={() =>
                      setExpandedProviders((current) => {
                        const next = new Set(current);
                        next.add(group.providerId);
                        return next;
                      })
                    }
                  >
                    Browse all {formatProvider(group.providerId)} models (
                    {group.hiddenCount} more)
                  </button>
                )}
              {index < groups.length - 1 && <ModelSelectorSeparator />}
            </ModelSelectorGroup>
          ))}
        </ModelSelectorList>
      </ModelSelectorContent>
    </ModelSelector>
  );
}

function modelSearchValue(model: UserModel): string {
  const providerId = model.provider_id ?? "unknown";
  return [
    model.name,
    model.id,
    stripProviderPrefix(model.id),
    providerId,
    formatProvider(providerId),
  ]
    .filter(Boolean)
    .join(" ");
}

function modelMatchesSearch(
  model: UserModel,
  normalizedSearch: string,
): boolean {
  return modelSearchValue(model).toLowerCase().includes(normalizedSearch);
}
