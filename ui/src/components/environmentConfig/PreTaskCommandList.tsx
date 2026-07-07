/**
 * PreTaskCommandList — editor for `PreTaskCommand[]`.
 *
 * Each PreTaskCommand has a structured shape: `{ command, name?, timeout_seconds?, failure_policy? }`.
 * This editor renders one row per command with the command string, optional
 * name, timeout, and failure-policy selector.
 */
import { useCallback } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import { Delete02Icon, PlusSignIcon } from "@hugeicons/core-free-icons";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { PreTaskCommand, PreTaskFailurePolicy } from "@/api/environmentConfig";
import {
  DEFAULT_PRE_TASK_FAILURE_POLICY,
  DEFAULT_PRE_TASK_TIMEOUT,
} from "@/api/environmentConfig";

interface PreTaskCommandRowProps {
  command: PreTaskCommand;
  onChange: (next: PreTaskCommand) => void;
  onRemove: () => void;
}

function PreTaskCommandRow({ command, onChange, onRemove }: PreTaskCommandRowProps) {
  return (
    <div className="rounded-md border bg-background/40 p-3">
      <div className="flex items-center justify-between gap-2 pb-2">
        <Input
          value={command.name ?? ""}
          onChange={(e) => onChange({ ...command, name: e.target.value || undefined })}
          placeholder="name (optional)"
          className="w-40 font-mono text-xs"
        />
        <Select
          value={command.failure_policy ?? DEFAULT_PRE_TASK_FAILURE_POLICY}
          onValueChange={(v) =>
            onChange({ ...command, failure_policy: v as PreTaskFailurePolicy })
          }
        >
          <SelectTrigger size="sm" className="w-32">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="blocking">blocking</SelectItem>
            <SelectItem value="best_effort">best_effort</SelectItem>
          </SelectContent>
        </Select>
        <Input
          type="number"
          value={command.timeout_seconds ?? DEFAULT_PRE_TASK_TIMEOUT}
          onChange={(e) =>
            onChange({
              ...command,
              timeout_seconds: parseInt(e.target.value, 10) || DEFAULT_PRE_TASK_TIMEOUT,
            })
          }
          className="w-20 font-mono text-xs"
          min={1}
          max={1800}
          title="Timeout (seconds)"
        />
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="h-7 w-7 p-0 text-muted-foreground hover:text-red-400"
          onClick={onRemove}
          title="Remove command"
        >
          <HugeiconsIcon icon={Delete02Icon} size={12} />
        </Button>
      </div>
      <Textarea
        value={command.command}
        placeholder="e.g. pip install -e ."
        onChange={(e) => onChange({ ...command, command: e.target.value })}
        className="min-h-[60px] font-mono text-xs"
      />
    </div>
  );
}

interface PreTaskCommandListProps {
  commands: PreTaskCommand[];
  onChange: (next: PreTaskCommand[]) => void;
  emptyHint?: string;
}

export function PreTaskCommandList({
  commands,
  onChange,
  emptyHint,
}: PreTaskCommandListProps) {
  const update = useCallback(
    (idx: number, next: PreTaskCommand) => {
      const copy = commands.slice();
      copy[idx] = next;
      onChange(copy);
    },
    [commands, onChange],
  );

  const remove = useCallback(
    (idx: number) => {
      const copy = commands.slice();
      copy.splice(idx, 1);
      onChange(copy);
    },
    [commands, onChange],
  );

  const add = useCallback(() => {
    onChange([
      ...commands,
      {
        command: "",
        timeout_seconds: DEFAULT_PRE_TASK_TIMEOUT,
        failure_policy: DEFAULT_PRE_TASK_FAILURE_POLICY,
      },
    ]);
  }, [commands, onChange]);

  return (
    <div className="flex flex-col gap-2">
      {commands.length === 0 && (
        <p className="text-xs text-muted-foreground">
          {emptyHint ?? "No pre-task commands configured."}
        </p>
      )}
      {commands.map((cmd, idx) => (
        <PreTaskCommandRow
          key={idx}
          command={cmd}
          onChange={(next) => update(idx, next)}
          onRemove={() => remove(idx)}
        />
      ))}
      <Button
        type="button"
        variant="outline"
        size="sm"
        className="h-8 w-fit gap-1.5 text-xs"
        onClick={add}
      >
        <HugeiconsIcon icon={PlusSignIcon} size={12} />
        Add pre-task command
      </Button>
    </div>
  );
}
