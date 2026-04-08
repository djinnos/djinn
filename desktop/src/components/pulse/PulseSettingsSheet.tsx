import { useCallback, useRef, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import { PlusSignIcon, Cancel01Icon, Settings01Icon } from "@hugeicons/core-free-icons";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { usePulseSettings } from "@/hooks/usePulseSettings";
import { cn } from "@/lib/utils";

interface PulseSettingsSheetProps {
  projectPath: string;
}

function ListEditor({
  label,
  description,
  items,
  placeholder,
  onAdd,
  onRemove,
  emptyText,
}: {
  label: string;
  description: string;
  items: string[];
  placeholder: string;
  onAdd: (value: string) => void;
  onRemove: (value: string) => void;
  emptyText: string;
}) {
  const [input, setInput] = useState("");

  function submit() {
    const trimmed = input.trim();
    if (!trimmed) return;
    onAdd(trimmed);
    setInput("");
  }

  return (
    <div className="space-y-2">
      <div>
        <p className="text-sm font-medium text-foreground">{label}</p>
        <p className="text-xs text-muted-foreground">{description}</p>
      </div>
      <ul className="space-y-1">
        {items.length === 0 ? (
          <li className="text-xs text-muted-foreground/70">{emptyText}</li>
        ) : (
          items.map((entry) => (
            <li
              key={entry}
              className="flex items-center justify-between gap-2 rounded-md bg-muted/40 px-2 py-1"
            >
              <span className="truncate font-mono text-xs text-foreground/80" title={entry}>
                {entry}
              </span>
              <Button
                size="icon-xs"
                variant="ghost"
                onClick={() => onRemove(entry)}
                aria-label={`Remove ${entry}`}
              >
                <HugeiconsIcon icon={Cancel01Icon} />
              </Button>
            </li>
          ))
        )}
      </ul>
      <div className="flex gap-2">
        <Input
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder={placeholder}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              submit();
            }
          }}
        />
        <Button size="sm" variant="outline" onClick={submit}>
          <HugeiconsIcon icon={PlusSignIcon} />
          Add
        </Button>
      </div>
    </div>
  );
}

export function PulseSettingsSheet({ projectPath }: PulseSettingsSheetProps) {
  const {
    settings,
    addExcludedPath,
    removeExcludedPath,
    addOrphanIgnore,
    removeOrphanIgnore,
  } = usePulseSettings(projectPath);

  const [savedFlash, setSavedFlash] = useState(false);
  const flashTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const flashSaved = useCallback(() => {
    setSavedFlash(true);
    if (flashTimer.current) clearTimeout(flashTimer.current);
    flashTimer.current = setTimeout(() => setSavedFlash(false), 1000);
  }, []);

  const wrap = useCallback(
    <A extends unknown[]>(fn: (...args: A) => void) =>
      (...args: A) => {
        fn(...args);
        flashSaved();
      },
    [flashSaved]
  );

  return (
    <Dialog>
      <DialogTrigger
        render={
          <Button variant="ghost" size="icon-sm" aria-label="Pulse settings" />
        }
      >
        <HugeiconsIcon icon={Settings01Icon} />
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <div className="flex items-center justify-between">
            <DialogTitle>Pulse settings</DialogTitle>
            <span
              className={cn(
                "text-[11px] text-emerald-400 transition-opacity duration-300",
                savedFlash ? "opacity-100" : "opacity-0"
              )}
            >
              Saved
            </span>
          </div>
          <DialogDescription>
            Calibrate what Pulse shows for this project. Settings are stored locally.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-6 pt-2">
          <ListEditor
            label="Excluded paths"
            description="Glob patterns hidden from every panel."
            items={settings.excluded_paths}
            placeholder="e.g. **/generated/**"
            onAdd={wrap(addExcludedPath)}
            onRemove={wrap(removeExcludedPath)}
            emptyText="No exclusions."
          />
          <ListEditor
            label="Ignored dead-code files"
            description="Files marked as not actually dead. Only affects the Dead code panel."
            items={settings.orphan_ignore}
            placeholder="e.g. src/api/public.rs"
            onAdd={wrap(addOrphanIgnore)}
            onRemove={wrap(removeOrphanIgnore)}
            emptyText="None ignored."
          />
        </div>
      </DialogContent>
    </Dialog>
  );
}
