import { useState, type FormEvent } from "react";
import { Delete02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  type AvailableMcpServer,
  type AvailableSkill,
  type BaseRole,
  type CreateAgentRequest,
} from "@/api/agents";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";

const BASE_ROLE_LABELS: Record<BaseRole, string> = {
  worker: "Worker",
  reviewer: "Task Reviewer",
  lead: "Lead",
  planner: "Planner",
  architect: "Architect",
};

const BASE_ROLES: BaseRole[] = ["worker", "reviewer", "lead", "planner", "architect"];

export interface AgentFormProps {
  initial?: Partial<Omit<CreateAgentRequest, "project_id">>;
  fixedBaseRole?: BaseRole;
  isDefaultEdit?: boolean;
  submitLabel: string;
  isBusy: boolean;
  availableMcpServers: AvailableMcpServer[];
  availableSkills: AvailableSkill[];
  onSubmit: (data: Omit<CreateAgentRequest, "project_id">) => void;
  onCancel: () => void;
}

function ReadOnlyMetadata({
  label,
  value,
  className,
}: {
  label: string;
  value: string;
  className?: string;
}) {
  return (
    <div className={cn("space-y-1.5 min-w-40", className)}>
      <div className="text-xs text-muted-foreground">{label}</div>
      <div className="rounded-md border border-border bg-muted/40 px-3 py-2 text-sm text-foreground">
        {value}
      </div>
    </div>
  );
}

export function AgentForm({
  initial,
  fixedBaseRole,
  isDefaultEdit = false,
  submitLabel,
  isBusy,
  availableMcpServers,
  availableSkills,
  onSubmit,
  onCancel,
}: AgentFormProps) {
  const [baseRole, setBaseRole] = useState<BaseRole>(
    fixedBaseRole ?? initial?.base_role ?? "worker",
  );
  const [name, setName] = useState(initial?.name ?? "");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [extensions, setExtensions] = useState(
    (initial?.system_prompt_extensions ?? []).join("\n"),
  );
  const [mcpServers, setMcpServers] = useState<string[]>(initial?.mcp_servers ?? []);
  const [skills, setSkills] = useState<string[]>(initial?.skills ?? []);

  let formTitle = "New specialist";
  if (isDefaultEdit) {
    formTitle = `Edit default ${BASE_ROLE_LABELS[baseRole]} instructions`;
  } else if (initial?.name) {
    formTitle = `Edit "${initial.name}"`;
  }

  const handleSubmit = (e: FormEvent) => {
    e.preventDefault();
    onSubmit({
      base_role: baseRole,
      name: name.trim(),
      description: description.trim(),
      system_prompt_extensions: extensions
        .split("\n")
        .map((line) => line.trim())
        .filter(Boolean),
      mcp_servers: mcpServers,
      skills,
    });
  };

  // MCP servers not yet assigned
  const unassignedMcpServers = availableMcpServers.filter(
    (s) => !mcpServers.includes(s.name),
  );
  // Skills not yet assigned
  const unassignedSkills = availableSkills.filter((s) => !skills.includes(s.name));

  return (
    <form onSubmit={handleSubmit} className="flex min-h-0 flex-1 flex-col">
      {/* Header bar */}
      <div className="shrink-0 border-b border-border px-6 py-4 space-y-4">
        <div className="flex items-center justify-between">
          <h3 className="text-lg font-semibold text-foreground">
            {formTitle}
          </h3>
          <div className="flex gap-2">
            <Button type="button" variant="outline" size="sm" onClick={onCancel} disabled={isBusy}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={isBusy || (!isDefaultEdit && !name.trim())}>
              {isBusy ? "Saving..." : submitLabel}
            </Button>
          </div>
        </div>

        {isDefaultEdit && (
          <p className="text-sm text-muted-foreground">
            Update the human-authored instructions and safe configuration used when Djinn
            automatically dispatches this project default. Identity fields are immutable.
          </p>
        )}

        {/* Compact metadata row */}
        <div className="flex flex-wrap items-end gap-4">
          {isDefaultEdit ? (
            <>
              <ReadOnlyMetadata label="Name" value={initial?.name ?? name} />
              <ReadOnlyMetadata label="Base role" value={BASE_ROLE_LABELS[baseRole]} />
              <ReadOnlyMetadata label="Default status" value="Project default" />
              {description && (
                <ReadOnlyMetadata
                  label="Description"
                  value={description}
                  className="min-w-72"
                />
              )}
            </>
          ) : !fixedBaseRole ? (
            <div className="space-y-1.5">
              <Label className="text-xs text-muted-foreground">Base role</Label>
              <div className="flex gap-1.5">
                {BASE_ROLES.map((role) => (
                  <button
                    key={role}
                    type="button"
                    onClick={() => setBaseRole(role)}
                    className={cn(
                      "rounded-md border px-2.5 py-1 text-xs transition-colors",
                      baseRole === role
                        ? "border-primary bg-primary text-primary-foreground"
                        : "border-border bg-card text-muted-foreground hover:bg-muted",
                    )}
                  >
                    {BASE_ROLE_LABELS[role]}
                  </button>
                ))}
              </div>
            </div>
          ) : null}

          {!isDefaultEdit && (
            <>
              <div className="space-y-1.5 flex-1 min-w-48">
                <Label htmlFor="role-name" className="text-xs text-muted-foreground">
                  Name
                </Label>
                <Input
                  id="role-name"
                  autoFocus
                  placeholder="e.g. Senior Backend Worker"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  required
                />
              </div>

              <div className="space-y-1.5 flex-1 min-w-48">
                <Label htmlFor="role-description" className="text-xs text-muted-foreground">
                  Description
                </Label>
                <Input
                  id="role-description"
                  placeholder="Short description of what this specialist does"
                  value={description}
                  onChange={(e) => setDescription(e.target.value)}
                />
              </div>
            </>
          )}
        </div>
      </div>

      {/* Scrollable body */}
      <div className="flex-1 min-h-0 overflow-y-auto p-6 pb-8 space-y-6">
        {/* System prompt extensions */}
        <div className="space-y-2">
          <Label htmlFor="role-extensions" className="text-xs text-muted-foreground block">
            {isDefaultEdit ? "Default instructions" : "System prompt extensions"}
          </Label>
          <Textarea
            id="role-extensions"
            placeholder={"You specialise in Rust systems programming.\nAlways write safe, idiomatic code.\n\nWhen reviewing code, focus on:\n- Memory safety\n- Error handling patterns\n- Idiomatic use of traits and generics"}
            value={extensions}
            onChange={(e) => setExtensions(e.target.value)}
            className="font-mono text-sm min-h-[200px] resize-none"
          />
        </div>

        {/* MCP Servers */}
        <div className="space-y-2">
          <div className="flex items-center justify-between">
            <Label className="text-xs text-muted-foreground">MCP Servers</Label>
            {unassignedMcpServers.length > 0 && (
              <select
                className="text-xs rounded-md border border-border bg-card px-2 py-1 text-foreground"
                value=""
                onChange={(e) => {
                  if (e.target.value) {
                    setMcpServers((prev) => [...prev, e.target.value]);
                  }
                }}
              >
                <option value="">Add server...</option>
                {unassignedMcpServers.map((s) => (
                  <option key={s.name} value={s.name}>
                    {s.name} ({s.transport})
                  </option>
                ))}
              </select>
            )}
          </div>
          {mcpServers.length === 0 ? (
            <p className="text-xs text-muted-foreground/60 italic">
              {availableMcpServers.length === 0
                ? "No MCP servers discovered. Add servers to mcp.json in your project."
                : "No servers assigned. Use the dropdown to add one."}
            </p>
          ) : (
            <div className="space-y-1.5">
              {mcpServers.map((serverName) => {
                const info = availableMcpServers.find((s) => s.name === serverName);
                return (
                  <div
                    key={serverName}
                    className="flex items-center gap-3 rounded-lg border bg-card px-3 py-2"
                  >
                    <div className="min-w-0 flex-1">
                      <span className="text-sm font-medium">{serverName}</span>
                      {info && (
                        <span className="ml-2 text-xs text-muted-foreground">
                          {info.transport}
                        </span>
                      )}
                    </div>
                    <Button
                      type="button"
                      variant="ghost"
                      size="sm"
                      onClick={() =>
                        setMcpServers((prev) => prev.filter((n) => n !== serverName))
                      }
                      className="shrink-0 text-destructive hover:text-destructive hover:bg-destructive/10 h-7 w-7 p-0"
                    >
                      <HugeiconsIcon icon={Delete02Icon} size={14} />
                    </Button>
                  </div>
                );
              })}
            </div>
          )}
        </div>

        {/* Skills */}
        <div className="space-y-2">
          <div className="flex items-center justify-between">
            <Label className="text-xs text-muted-foreground">Skills</Label>
            {unassignedSkills.length > 0 && (
              <select
                className="text-xs rounded-md border border-border bg-card px-2 py-1 text-foreground"
                value=""
                onChange={(e) => {
                  if (e.target.value) {
                    setSkills((prev) => [...prev, e.target.value]);
                  }
                }}
              >
                <option value="">Add skill...</option>
                {unassignedSkills.map((s) => (
                  <option key={s.name} value={s.name}>
                    {s.name}
                    {s.description ? ` — ${s.description}` : ""}
                  </option>
                ))}
              </select>
            )}
          </div>
          {skills.length === 0 ? (
            <p className="text-xs text-muted-foreground/60 italic">
              {availableSkills.length === 0
                ? "No skills discovered. Add SKILL.md directories under .claude/skills/ or .opencode/skills/."
                : "No skills assigned. Use the dropdown to add one."}
            </p>
          ) : (
            <div className="space-y-1.5">
              {skills.map((skillName) => {
                const info = availableSkills.find((s) => s.name === skillName);
                return (
                  <div
                    key={skillName}
                    className="flex items-center gap-3 rounded-lg border bg-card px-3 py-2"
                  >
                    <div className="min-w-0 flex-1">
                      <span className="text-sm font-medium">{skillName}</span>
                      {info?.description && (
                        <span className="ml-2 text-xs text-muted-foreground truncate">
                          {info.description}
                        </span>
                      )}
                    </div>
                    <Button
                      type="button"
                      variant="ghost"
                      size="sm"
                      onClick={() => setSkills((prev) => prev.filter((n) => n !== skillName))}
                      className="shrink-0 text-destructive hover:text-destructive hover:bg-destructive/10 h-7 w-7 p-0"
                    >
                      <HugeiconsIcon icon={Delete02Icon} size={14} />
                    </Button>
                  </div>
                );
              })}
            </div>
          )}
        </div>
      </div>
    </form>
  );
}
