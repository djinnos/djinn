/**
 * The visual filter controls (project / epic dropdowns and the owner avatar)
 * shared by the board filter header. Logic + state live in `boardFilters`.
 */
import { useMemo, useState } from "react";
import type { Epic, Project } from "@/api/types";
import { HugeiconsIcon } from "@hugeicons/react";
import { ArrowDown01Icon, Tick02Icon } from "@hugeicons/core-free-icons";
import { UserAvatar } from "@/components/UserAvatar";
import { cn } from "@/lib/utils";
import {
  ModelSelector as SelectorRoot,
  ModelSelectorContent,
  ModelSelectorEmpty,
  ModelSelectorInput,
  ModelSelectorItem,
  ModelSelectorList,
  ModelSelectorName,
  ModelSelectorTrigger,
} from "@/components/ai-elements/model-selector";
import { EPIC_STATUS_GROUPS, getEpicEmoji } from "./boardFilters";

/** Small round avatar for an owner-filter option. */
export const OwnerAvatar = UserAvatar;

export function ProjectFilter({
  projects,
  selected,
  onChange,
}: {
  projects: Project[];
  selected: string[];
  onChange: (ids: string[]) => void;
}) {
  const [open, setOpen] = useState(false);

  const sorted = useMemo(
    () =>
      [...projects].sort((a, b) => (a.name ?? "").localeCompare(b.name ?? "")),
    [projects],
  );

  const toggle = (id: string) => {
    onChange(
      selected.includes(id)
        ? selected.filter((s) => s !== id)
        : [...selected, id],
    );
  };

  const label =
    selected.length > 0
      ? `${selected.length} project${selected.length > 1 ? "s" : ""}`
      : "All projects";

  return (
    <SelectorRoot open={open} onOpenChange={setOpen}>
      <ModelSelectorTrigger
        className={cn(
          "flex h-8 items-center gap-1.5 rounded-lg border border-input px-3 text-sm transition-colors dark:bg-input/30",
          selected.length > 0 ? "text-foreground" : "text-muted-foreground",
        )}
      >
        {label}
        <HugeiconsIcon
          icon={ArrowDown01Icon}
          size={12}
          className="shrink-0 text-muted-foreground"
        />
      </ModelSelectorTrigger>

      <ModelSelectorContent title="Filter by project">
        <ModelSelectorInput placeholder="Search projects…" />
        <ModelSelectorList>
          <ModelSelectorEmpty>No projects found.</ModelSelectorEmpty>
          {sorted.map((project) => (
            <ModelSelectorItem
              key={project.id}
              searchValue={`${project.name} ${project.github_owner}/${project.github_repo}`}
              onSelect={() => toggle(project.id)}
            >
              <ModelSelectorName>{project.name}</ModelSelectorName>
              <span className="shrink-0 text-xs text-muted-foreground">
                {project.github_owner}/{project.github_repo}
              </span>
              {selected.includes(project.id) && (
                <HugeiconsIcon
                  icon={Tick02Icon}
                  size={14}
                  className="shrink-0 text-primary"
                />
              )}
            </ModelSelectorItem>
          ))}
        </ModelSelectorList>
      </ModelSelectorContent>
    </SelectorRoot>
  );
}

export function EpicFilter({
  epics,
  selected,
  onChange,
}: {
  epics: Epic[];
  selected: string[];
  onChange: (ids: string[]) => void;
}) {
  const [open, setOpen] = useState(false);

  const grouped = useMemo(() => {
    const map = new Map<string, Epic[]>();
    for (const epic of epics) {
      const status = epic.status ?? "open";
      const list = map.get(status) ?? [];
      list.push(epic);
      map.set(status, list);
    }
    return EPIC_STATUS_GROUPS.filter((g) => map.has(g.key)).map((g) => ({
      ...g,
      items: map.get(g.key)!,
    }));
  }, [epics]);

  const toggle = (id: string) => {
    onChange(
      selected.includes(id)
        ? selected.filter((s) => s !== id)
        : [...selected, id],
    );
  };

  const label =
    selected.length > 0
      ? `${selected.length} epic${selected.length > 1 ? "s" : ""}`
      : "All epics";

  return (
    <SelectorRoot open={open} onOpenChange={setOpen}>
      <ModelSelectorTrigger
        className={cn(
          "flex h-8 items-center gap-1.5 rounded-lg border border-input px-3 text-sm transition-colors dark:bg-input/30",
          selected.length > 0 ? "text-foreground" : "text-muted-foreground",
        )}
      >
        {label}
        <HugeiconsIcon
          icon={ArrowDown01Icon}
          size={12}
          className="shrink-0 text-muted-foreground"
        />
      </ModelSelectorTrigger>

      <ModelSelectorContent title="Filter by epic">
        <ModelSelectorInput placeholder="Search epics…" />
        <ModelSelectorList>
          <ModelSelectorEmpty>No epics found.</ModelSelectorEmpty>
          {grouped.map((group) => (
            <div
              key={group.key}
              data-slot="command-group"
              className="text-foreground overflow-hidden p-1"
            >
              <div
                data-slot="command-group-heading"
                className="px-2 py-1.5 text-xs font-medium text-muted-foreground"
              >
                {group.label}
              </div>
              <div data-slot="command-group-items">
                {group.items.map((epic) => (
                  <ModelSelectorItem
                    key={epic.id}
                    searchValue={epic.title ?? ""}
                    onSelect={() => toggle(epic.id)}
                  >
                    <span className="shrink-0 text-xs leading-none">
                      {getEpicEmoji(epic)}
                    </span>
                    <ModelSelectorName>{epic.title}</ModelSelectorName>
                    {selected.includes(epic.id) && (
                      <HugeiconsIcon
                        icon={Tick02Icon}
                        size={14}
                        className="shrink-0 text-primary"
                      />
                    )}
                  </ModelSelectorItem>
                ))}
              </div>
            </div>
          ))}
        </ModelSelectorList>
      </ModelSelectorContent>
    </SelectorRoot>
  );
}
