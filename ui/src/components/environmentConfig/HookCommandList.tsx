/**
 * HookCommandList — editor for the three-way HookCommand union.
 *
 * Mirrors `djinn_stack::environment::HookCommand`:
 *   Shell(String)        → "shell" form: single textarea
 *   Exec(Vec<String>)    → "exec" form: one input per argv token
 *   Parallel(Map<...>)   → "parallel" form: name → nested HookCommand
 *
 * The parallel shape is recursive; for UI tractability we cap the
 * recursion to one level and render nested entries as shell strings.
 * That matches what the Rust side accepts (inner `HookCommand` can be
 * any variant, but in practice the typical author's use is a flat
 * name-map of shell commands).
 */
import { useCallback } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import { Delete02Icon, PlusSignIcon } from "@hugeicons/core-free-icons";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import type { HookCommand } from "@/api/environmentConfig";

export type HookKind = "shell" | "exec" | "parallel";

function kindOf(hook: HookCommand): HookKind {
  if (typeof hook === "string") return "shell";
  if (Array.isArray(hook)) return "exec";
  return "parallel";
}

function emptyHook(kind: HookKind): HookCommand {
  switch (kind) {
    case "shell":
      return "";
    case "exec":
      return [""];
    case "parallel":
      return {};
  }
}

interface HookCommandRowProps {
  hook: HookCommand;
  onChange: (next: HookCommand) => void;
  onRemove: () => void;
}

function HookCommandRow({ hook, onChange, onRemove }: HookCommandRowProps) {
  const kind = kindOf(hook);

  const handleKindChange = (next: string) => {
    if (next === kind) return;
    onChange(emptyHook(next as HookKind));
  };

  return (
    <div className="rounded-md border bg-background/40 p-3">
      <div className="flex items-center justify-between gap-2 pb-2">
        <Select value={kind} onValueChange={(v) => typeof v === "string" && handleKindChange(v)}>
          <SelectTrigger size="sm" className="w-32">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="shell">shell</SelectItem>
            <SelectItem value="exec">exec (argv)</SelectItem>
            <SelectItem value="parallel">parallel</SelectItem>
          </SelectContent>
        </Select>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="h-7 gap-1 px-2 text-muted-foreground hover:text-red-400"
          onClick={onRemove}
          title="Remove hook"
        >
          <HugeiconsIcon icon={Delete02Icon} size={14} />
        </Button>
      </div>

      {kind === "shell" && (
        <Textarea
          value={hook as string}
          placeholder="e.g. cargo fetch"
          onChange={(e) => onChange(e.target.value)}
          className="min-h-[60px] font-mono text-xs"
        />
      )}

      {kind === "exec" && <ExecEditor argv={hook as string[]} onChange={onChange} />}

      {kind === "parallel" && (
        <ParallelEditor map={hook as Record<string, HookCommand>} onChange={onChange} />
      )}
    </div>
  );
}

function ExecEditor({ argv, onChange }: { argv: string[]; onChange: (next: HookCommand) => void }) {
  const update = (idx: number, value: string) => {
    const next = argv.slice();
    next[idx] = value;
    onChange(next);
  };
  const add = () => onChange([...argv, ""]);
  const remove = (idx: number) => {
    const next = argv.slice();
    next.splice(idx, 1);
    onChange(next.length === 0 ? [""] : next);
  };
  return (
    <div className="flex flex-col gap-1.5">
      {argv.map((token, idx) => (
        <div key={idx} className="flex items-center gap-1.5">
          <Input
            value={token}
            onChange={(e) => update(idx, e.target.value)}
            placeholder={idx === 0 ? "bash" : `arg ${idx}`}
            className="font-mono text-xs"
          />
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-7 w-7 p-0 text-muted-foreground hover:text-red-400"
            onClick={() => remove(idx)}
            title="Remove argument"
          >
            <HugeiconsIcon icon={Delete02Icon} size={12} />
          </Button>
        </div>
      ))}
      <Button
        type="button"
        variant="ghost"
        size="sm"
        className="h-7 w-fit gap-1 px-2 text-xs text-muted-foreground"
        onClick={add}
      >
        <HugeiconsIcon icon={PlusSignIcon} size={12} />
        Add argument
      </Button>
    </div>
  );
}

function ParallelEditor({
  map,
  onChange,
}: {
  map: Record<string, HookCommand>;
  onChange: (next: HookCommand) => void;
}) {
  const entries = Object.entries(map);

  const updateName = (oldName: string, newName: string) => {
    if (!newName || newName === oldName) return;
    const next: Record<string, HookCommand> = {};
    for (const [k, v] of entries) next[k === oldName ? newName : k] = v;
    onChange(next);
  };
  const updateValue = (name: string, value: string) => {
    const next: Record<string, HookCommand> = { ...map, [name]: value };
    onChange(next);
  };
  const remove = (name: string) => {
    const next = { ...map };
    delete next[name];
    onChange(next);
  };
  const add = () => {
    let name = "step";
    let i = 1;
    while (name in map) {
      i += 1;
      name = `step${i}`;
    }
    onChange({ ...map, [name]: "" });
  };

  return (
    <div className="flex flex-col gap-2">
      <p className="text-[11px] text-muted-foreground">
        Named steps run in parallel. Each value is treated as a shell command.
      </p>
      {entries.map(([name, value]) => (
        <div key={name} className="flex items-start gap-1.5">
          <Input
            defaultValue={name}
            onBlur={(e) => updateName(name, e.target.value.trim())}
            placeholder="name"
            className="w-40 font-mono text-xs"
          />
          <Textarea
            value={typeof value === "string" ? value : JSON.stringify(value)}
            onChange={(e) => updateValue(name, e.target.value)}
            className="min-h-[48px] flex-1 font-mono text-xs"
          />
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-7 w-7 p-0 text-muted-foreground hover:text-red-400"
            onClick={() => remove(name)}
          >
            <HugeiconsIcon icon={Delete02Icon} size={12} />
          </Button>
        </div>
      ))}
      <Button
        type="button"
        variant="ghost"
        size="sm"
        className="h-7 w-fit gap-1 px-2 text-xs text-muted-foreground"
        onClick={add}
      >
        <HugeiconsIcon icon={PlusSignIcon} size={12} />
        Add parallel step
      </Button>
    </div>
  );
}

interface HookCommandListProps {
  hooks: HookCommand[];
  onChange: (next: HookCommand[]) => void;
  emptyHint?: string;
}

export function HookCommandList({ hooks, onChange, emptyHint }: HookCommandListProps) {
  const update = useCallback(
    (idx: number, next: HookCommand) => {
      const copy = hooks.slice();
      copy[idx] = next;
      onChange(copy);
    },
    [hooks, onChange],
  );

  const remove = useCallback(
    (idx: number) => {
      const copy = hooks.slice();
      copy.splice(idx, 1);
      onChange(copy);
    },
    [hooks, onChange],
  );

  const add = useCallback(() => {
    onChange([...hooks, ""]);
  }, [hooks, onChange]);

  return (
    <div className="flex flex-col gap-2">
      {hooks.length === 0 && (
        <p className="text-xs text-muted-foreground">{emptyHint ?? "No hooks configured."}</p>
      )}
      {hooks.map((hook, idx) => (
        <HookCommandRow
          key={idx}
          hook={hook}
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
        Add hook
      </Button>
    </div>
  );
}
