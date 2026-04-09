import { useCallback, useEffect, useState } from "react";
import { useSelectedProject } from "@/stores/useProjectStore";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { ConfirmButton } from "@/components/ConfirmButton";
import { InlineError } from "@/components/InlineError";
import { cn } from "@/lib/utils";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Delete02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  type BaseRole,
  type CreateAgentRequest,
  type LearnedPromptAmendment,
  type LearnedPromptHistory,
  type Agent,
  type AvailableMcpServer,
  type AvailableSkill,
  clearLearnedPrompt,
  createAgent,
  deleteAgent,
  fetchAvailableMcpServers,
  fetchAvailableSkills,
  fetchLearnedPromptHistory,
  fetchAgents,
  updateAgent,
} from "@/api/agents";

const BASE_ROLE_LABELS: Record<BaseRole, string> = {
  worker: "Worker",
  reviewer: "Task Reviewer",
  lead: "Lead",
  planner: "Planner",
};

const BASE_ROLES: BaseRole[] = ["worker", "reviewer", "lead", "planner"];

// ── Role Form ────────────────────────────────────────────────────────────────

interface AgentFormProps {
  initial?: Partial<Omit<CreateAgentRequest, "project_id">>;
  fixedBaseRole?: BaseRole;
  submitLabel: string;
  isBusy: boolean;
  availableMcpServers: AvailableMcpServer[];
  availableSkills: AvailableSkill[];
  onSubmit: (data: Omit<CreateAgentRequest, "project_id">) => void;
  onCancel: () => void;
}

function AgentForm({ initial, fixedBaseRole, submitLabel, isBusy, availableMcpServers, availableSkills, onSubmit, onCancel }: AgentFormProps) {
  const [baseRole, setBaseRole] = useState<BaseRole>(fixedBaseRole ?? initial?.base_role ?? "worker");
  const [name, setName] = useState(initial?.name ?? "");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [extensions, setExtensions] = useState(
    (initial?.system_prompt_extensions ?? []).join("\n"),
  );
  const [mcpServers, setMcpServers] = useState<string[]>(initial?.mcp_servers ?? []);
  const [skills, setSkills] = useState<string[]>(initial?.skills ?? []);
  const [verificationCommand, setVerificationCommand] = useState(initial?.verification_command ?? "");

  const handleSubmit = (e: React.FormEvent) => {
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
      verification_command: verificationCommand.trim() || null,
    });
  };

  // MCP servers not yet assigned
  const unassignedMcpServers = availableMcpServers.filter(
    (s) => !mcpServers.includes(s.name),
  );
  // Skills not yet assigned
  const unassignedSkills = availableSkills.filter(
    (s) => !skills.includes(s.name),
  );

  return (
    <form onSubmit={handleSubmit} className="flex min-h-0 flex-1 flex-col">
      {/* Header bar */}
      <div className="shrink-0 border-b border-border px-6 py-4 space-y-4">
        <div className="flex items-center justify-between">
          <h3 className="text-lg font-semibold text-foreground">
            {initial?.name ? `Edit "${initial.name}"` : "New specialist"}
          </h3>
          <div className="flex gap-2">
            <Button type="button" variant="outline" size="sm" onClick={onCancel} disabled={isBusy}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={isBusy || !name.trim()}>
              {isBusy ? "Saving..." : submitLabel}
            </Button>
          </div>
        </div>

        {/* Compact metadata row */}
        <div className="flex flex-wrap items-end gap-4">
          {!fixedBaseRole && (
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
          )}

          <div className="space-y-1.5 flex-1 min-w-48">
            <Label htmlFor="role-name" className="text-xs text-muted-foreground">Name</Label>
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
            <Label htmlFor="role-description" className="text-xs text-muted-foreground">Description</Label>
            <Input
              id="role-description"
              placeholder="Short description of what this specialist does"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
          </div>
        </div>
      </div>

      {/* Scrollable body */}
      <div className="flex-1 min-h-0 overflow-y-auto p-6 pb-8 space-y-6">
        {/* System prompt extensions */}
        <div className="space-y-2">
          <Label htmlFor="role-extensions" className="text-xs text-muted-foreground block">
            System prompt extensions
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
                    {s.name}{s.description ? ` — ${s.description}` : ""}
                  </option>
                ))}
              </select>
            )}
          </div>
          {skills.length === 0 ? (
            <p className="text-xs text-muted-foreground/60 italic">
              {availableSkills.length === 0
                ? "No skills discovered. Add .md files to .djinn/skills/ in your project."
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
                      onClick={() =>
                        setSkills((prev) => prev.filter((n) => n !== skillName))
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

        {/* Verification command */}
        <div className="space-y-2">
          <Label htmlFor="role-verification" className="text-xs text-muted-foreground block">
            Verification command
          </Label>
          <Input
            id="role-verification"
            placeholder="e.g. cargo test -- --nocapture"
            value={verificationCommand}
            onChange={(e) => setVerificationCommand(e.target.value)}
            className="font-mono text-sm"
          />
          <p className="text-xs text-muted-foreground/60">
            Custom command to verify this agent's work. Leave empty to use project defaults.
          </p>
        </div>
      </div>
    </form>
  );
}

// ── Learned Prompt Section ────────────────────────────────────────────────────

function formatMetricDelta(before: number, after: number): string {
  const delta = after - before;
  const sign = delta >= 0 ? "+" : "";
  return `${sign}${delta.toFixed(2)}`;
}

function AmendmentEntry({ amendment }: { amendment: LearnedPromptAmendment }) {
  const HIDDEN_METRICS = new Set(["agent_name", "completed_task_count"]);
  const metricKeys = Object.keys(amendment.metrics_before).filter((key) => {
    if (HIDDEN_METRICS.has(key)) return false;
    const before = Number(amendment.metrics_before[key]);
    const after = Number(amendment.metrics_after[key]);
    return !Number.isNaN(before) && !Number.isNaN(after);
  });

  return (
    <div className="rounded-lg border border-border/40 p-3 space-y-2 text-xs">
      <div className="flex items-center gap-2">
        <span className="text-muted-foreground">
          {new Date(amendment.created_at).toLocaleString()}
        </span>
        {amendment.metrics_after.completed_task_count != null && (
          <span className="text-muted-foreground/60 font-mono">
            {Number(amendment.metrics_after.completed_task_count)} tasks
          </span>
        )}
      </div>

      <div className="prose prose-sm max-w-none dark:prose-invert text-xs leading-relaxed">
        <ReactMarkdown remarkPlugins={[remarkGfm]}>
          {amendment.proposed_text}
        </ReactMarkdown>
      </div>

      {metricKeys.length > 0 && (
        <div className="flex flex-wrap gap-3 text-muted-foreground">
          {metricKeys.map((key) => {
            const before = Number(amendment.metrics_before[key] ?? 0);
            const after = Number(amendment.metrics_after[key] ?? 0);
            const delta = after - before;
            return (
              <span key={key}>
                {key}:{" "}
                <span className="text-foreground font-mono">
                  {before.toFixed(2)} → {after.toFixed(2)}
                </span>{" "}
                <span
                  className={cn(
                    "font-mono",
                    delta > 0
                      ? "text-green-600 dark:text-green-400"
                      : delta < 0
                        ? "text-red-600 dark:text-red-400"
                        : "text-muted-foreground",
                  )}
                >
                  ({formatMetricDelta(before, after)})
                </span>
              </span>
            );
          })}
        </div>
      )}
    </div>
  );
}

function DiscardedSection({ discarded }: { discarded: LearnedPromptAmendment[] }) {
  const [open, setOpen] = useState(false);

  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex items-center gap-1.5 text-xs text-muted-foreground hover:text-foreground transition-colors"
      >
        Discarded ({discarded.length})
        <span className="text-muted-foreground/60">{open ? "▴" : "▾"}</span>
      </button>
      {open && (
        <div className="mt-1 space-y-4">
          {discarded.map((amendment) => (
            <AmendmentEntry key={amendment.id} amendment={amendment} />
          ))}
        </div>
      )}
    </div>
  );
}

interface LearnedPromptSectionProps {
  role: Agent;
  onCleared: () => void;
}

function LearnedPromptSection({ role, onCleared }: LearnedPromptSectionProps) {
  const [expanded, setExpanded] = useState(false);
  const [history, setHistory] = useState<LearnedPromptHistory | null>(null);
  const [loadingHistory, setLoadingHistory] = useState(false);
  const [historyError, setHistoryError] = useState<string | null>(null);
  const [clearing, setClearing] = useState(false);

  const loadHistory = useCallback(async () => {
    setLoadingHistory(true);
    setHistoryError(null);
    try {
      const data = await fetchLearnedPromptHistory(role.id);
      setHistory(data);
    } catch (err) {
      setHistoryError(err instanceof Error ? err.message : "Failed to load history");
    } finally {
      setLoadingHistory(false);
    }
  }, [role.id]);

  useEffect(() => {
    if (expanded && !history && !loadingHistory) {
      void loadHistory();
    }
  }, [expanded, history, loadingHistory, loadHistory]);

  const handleClear = async () => {
    setClearing(true);
    try {
      await clearLearnedPrompt(role.id);
      setHistory(null);
      setExpanded(false);
      onCleared();
    } catch (err) {
      setHistoryError(err instanceof Error ? err.message : "Failed to clear learned prompt");
    } finally {
      setClearing(false);
    }
  };

  const hasLearnedPrompt = !!role.learned_prompt;

  return (
    <div className="border-t border-border pt-2 mt-2">
      <div className="flex items-center justify-between gap-2">
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className="flex items-center gap-1.5 text-xs text-muted-foreground hover:text-foreground transition-colors"
        >
          <span
            className={cn(
              "rounded-full w-1.5 h-1.5 shrink-0",
              hasLearnedPrompt ? "bg-blue-500" : "bg-muted-foreground/40",
            )}
          />
          Learned prompt
          {hasLearnedPrompt && (
            <span className="text-blue-600 dark:text-blue-400">(active)</span>
          )}
          <span className="text-muted-foreground/60">{expanded ? "▴" : "▾"}</span>
        </button>

        {hasLearnedPrompt && (
          <ConfirmButton
            title="Clear learned prompt"
            description={`Clear the learned prompt for "${role.name}"? The auto-improvement history will be preserved.`}
            confirmLabel="Clear"
            onConfirm={() => void handleClear()}
            size="sm"
            variant="ghost"
            disabled={clearing}
          >
            {clearing ? "Clearing..." : "Clear"}
          </ConfirmButton>
        )}
      </div>

      {expanded && (
        <div className="mt-3 space-y-3">
          {loadingHistory && (
            <p className="text-xs text-muted-foreground">Loading history...</p>
          )}
          {historyError && (
            <p className="text-xs text-red-500">{historyError}</p>
          )}
          {history && !loadingHistory && (() => {
            const kept = [...history.amendments]
              .filter((a) => a.action === "keep")
              .sort((a, b) => new Date(a.created_at).getTime() - new Date(b.created_at).getTime());
            const discarded = [...history.amendments]
              .filter((a) => a.action !== "keep")
              .sort((a, b) => new Date(b.created_at).getTime() - new Date(a.created_at).getTime());

            if (kept.length === 0 && discarded.length === 0) {
              return <p className="text-xs text-muted-foreground italic">No amendments yet.</p>;
            }

            return (
              <>
                {kept.length > 0 && (
                  <div>
                    <p className="text-xs font-medium text-muted-foreground mb-1">
                      Kept ({kept.length})
                    </p>
                    <div className="space-y-4">
                      {kept.map((amendment) => (
                        <AmendmentEntry key={amendment.id} amendment={amendment} />
                      ))}
                    </div>
                  </div>
                )}
                {discarded.length > 0 && (
                  <DiscardedSection discarded={discarded} />
                )}
              </>
            );
          })()}
        </div>
      )}
    </div>
  );
}

// ── Role Card ────────────────────────────────────────────────────────────────

interface AgentCardProps {
  role: Agent;
  onEdit: () => void;
  onDelete: () => void;
  onRoleCleared: () => void;
  isDeleting: boolean;
}

function AgentCard({ role, onEdit, onDelete, onRoleCleared, isDeleting }: AgentCardProps) {
  return (
    <div className="rounded-lg border border-border bg-card p-4 space-y-2">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="font-medium truncate">{role.name}</span>
            {role.is_default && (
              <span className="shrink-0 rounded-full bg-muted px-2 py-0.5 text-xs text-muted-foreground">
                default
              </span>
            )}
          </div>
          {role.description && (
            <p className="text-xs text-muted-foreground mt-0.5 truncate">{role.description}</p>
          )}
        </div>

        {!role.is_default && (
          <div className="flex items-center gap-2 shrink-0">
            <Button variant="outline" size="sm" onClick={onEdit}>
              Edit
            </Button>
            <ConfirmButton
              title="Delete specialist"
              description={`Delete "${role.name}"? This cannot be undone.`}
              confirmLabel="Delete"
              onConfirm={onDelete}
              size="sm"
              disabled={isDeleting}
            >
              {isDeleting ? "Deleting..." : "Delete"}
            </ConfirmButton>
          </div>
        )}
      </div>

      {role.system_prompt_extensions.length > 0 && (
        <div className="rounded-md bg-muted px-3 py-2 text-xs font-mono text-muted-foreground space-y-0.5">
          {role.system_prompt_extensions.map((ext, i) => (
            <p key={i}>{ext}</p>
          ))}
        </div>
      )}

      {((role.mcp_servers?.length ?? 0) > 0 || (role.skills?.length ?? 0) > 0) && (
        <div className="flex flex-wrap gap-1.5">
          {(role.mcp_servers ?? []).map((name) => (
            <span
              key={`mcp-${name}`}
              className="inline-flex items-center rounded-full bg-blue-500/10 px-2 py-0.5 text-xs text-blue-700 dark:text-blue-300"
            >
              {name}
            </span>
          ))}
          {(role.skills ?? []).map((name) => (
            <span
              key={`skill-${name}`}
              className="inline-flex items-center rounded-full bg-purple-500/10 px-2 py-0.5 text-xs text-purple-700 dark:text-purple-300"
            >
              {name}
            </span>
          ))}
        </div>
      )}

      <LearnedPromptSection role={role} onCleared={onRoleCleared} />
    </div>
  );
}

// ── Main Component ───────────────────────────────────────────────────────────

export function AgentRoles() {
  const project = useSelectedProject();
  const [roles, setRoles] = useState<Agent[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  // Available MCP servers and skills for the project
  const [availableMcpServers, setAvailableMcpServers] = useState<AvailableMcpServer[]>([]);
  const [availableSkills, setAvailableSkills] = useState<AvailableSkill[]>([]);

  // Create form
  const [isCreating, setIsCreating] = useState(false);
  const [createBusy, setCreateBusy] = useState(false);

  // Edit state: role id → draft
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editBusy, setEditBusy] = useState(false);

  // Deleting id
  const [deletingId, setDeletingId] = useState<string | null>(null);

  const loadRoles = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await fetchAgents(project?.id);
      setRoles(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load agents");
    } finally {
      setLoading(false);
    }
  }, [project?.id]);

  // Load available MCP servers and skills when project changes or form opens
  const loadAvailableOptions = useCallback(async () => {
    if (!project?.id) return;
    try {
      const [servers, sk] = await Promise.all([
        fetchAvailableMcpServers(project.id),
        fetchAvailableSkills(project.id),
      ]);
      setAvailableMcpServers(servers);
      setAvailableSkills(sk);
    } catch {
      // Non-fatal — form still works, just without suggestions
    }
  }, [project?.id]);

  useEffect(() => {
    void loadRoles();
  }, [loadRoles]);

  // Load available MCP servers and skills when entering create/edit mode
  useEffect(() => {
    if (isCreating || editingId) {
      void loadAvailableOptions();
    }
  }, [isCreating, editingId, loadAvailableOptions]);

  const handleCreate = async (data: Omit<CreateAgentRequest, "project_id">) => {
    if (!project) return;
    setCreateBusy(true);
    try {
      const role = await createAgent({ ...data, project_id: project.id });
      setRoles((prev) => [...prev, role]);
      setIsCreating(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create agent");
    } finally {
      setCreateBusy(false);
    }
  };

  const handleUpdate = async (id: string, data: Omit<CreateAgentRequest, "project_id">) => {
    setEditBusy(true);
    try {
      const updated = await updateAgent(id, {
        name: data.name,
        description: data.description,
        system_prompt_extensions: data.system_prompt_extensions,
        mcp_servers: data.mcp_servers,
        skills: data.skills,
        verification_command: data.verification_command,
      });
      setRoles((prev) => prev.map((r) => (r.id === id ? updated : r)));
      setEditingId(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to update agent");
    } finally {
      setEditBusy(false);
    }
  };

  const handleDelete = async (id: string) => {
    setDeletingId(id);
    try {
      await deleteAgent(id);
      setRoles((prev) => prev.filter((r) => r.id !== id));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to delete agent");
    } finally {
      setDeletingId(null);
    }
  };

  // Group roles by base_role
  const grouped = BASE_ROLES.map((baseRole) => ({
    baseRole,
    label: BASE_ROLE_LABELS[baseRole],
    roles: roles.filter((r) => r.base_role === baseRole),
  })).filter((g) => g.roles.length > 0);

  // Full-page form takeover for create/edit
  const editingRole = editingId ? roles.find((r) => r.id === editingId) : null;

  if (isCreating) {
    return (
      <div className="flex h-full flex-col rounded-lg border border-border bg-card overflow-hidden">
        <AgentForm
          submitLabel="Create"
          isBusy={createBusy}
          availableMcpServers={availableMcpServers}
          availableSkills={availableSkills}
          onSubmit={(data) => void handleCreate(data)}
          onCancel={() => setIsCreating(false)}
        />
      </div>
    );
  }

  if (editingRole) {
    return (
      <div className="flex h-full flex-col rounded-lg border border-border bg-card overflow-hidden">
        <AgentForm
          initial={{
            base_role: editingRole.base_role,
            name: editingRole.name,
            description: editingRole.description,
            system_prompt_extensions: editingRole.system_prompt_extensions,
            mcp_servers: editingRole.mcp_servers,
            skills: editingRole.skills,
            verification_command: editingRole.verification_command,
          }}
          fixedBaseRole={editingRole.base_role}
          submitLabel="Save"
          isBusy={editBusy}
          availableMcpServers={availableMcpServers}
          availableSkills={availableSkills}
          onSubmit={(data) => void handleUpdate(editingRole.id, data)}
          onCancel={() => setEditingId(null)}
        />
      </div>
    );
  }

  if (loading) {
    return (
      <div className="rounded-lg border border-border bg-card p-6 text-sm text-muted-foreground">
        Loading roles...
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between shrink-0">
        <div>
          <h2 className="text-lg font-semibold">Agent Roles</h2>
          <p className="text-sm text-muted-foreground">
            Manage specialist roles that extend base agent behaviour.
          </p>
        </div>
        <Button onClick={() => setIsCreating(true)}>New Specialist</Button>
      </div>

      {error && <InlineError message={error} onRetry={() => void loadRoles()} />}

      {roles.length === 0 && !isCreating ? (
        <div className="rounded-lg border border-dashed border-border p-8 text-center text-sm text-muted-foreground">
          No roles configured yet. Create a specialist to extend a base role.
        </div>
      ) : (
        <div className="space-y-6">
          {grouped.map(({ baseRole, label, roles: groupRoles }) => (
            <div key={baseRole}>
              <h3 className="text-sm font-semibold text-muted-foreground uppercase tracking-wide mb-2">
                {label}
              </h3>
              <div className="space-y-2">
                {groupRoles.map((role) => (
                  <AgentCard
                    key={role.id}
                    role={role}
                    onEdit={() => setEditingId(role.id)}
                    onDelete={() => void handleDelete(role.id)}
                    onRoleCleared={() => void loadRoles()}
                    isDeleting={deletingId === role.id}
                  />
                ))}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
