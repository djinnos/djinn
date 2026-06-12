import { useCallback, useEffect, useState } from "react";
import { useSelectedProject } from "@/stores/useProjectStore";
import { Button } from "@/components/ui/button";
import { ConfirmButton } from "@/components/ConfirmButton";
import { InlineError } from "@/components/InlineError";
import { useAuthUser } from "@/components/AuthGate";
import { cn } from "@/lib/utils";
import { getAgentIdentity } from "@/lib/agentIdentity";
import { AgentForm } from "@/components/agentRoles/AgentForm";
import { Delete02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  type CreateAgentRequest,
  type Agent,
  type AvailableMcpServer,
  type AvailableSkill,
  createAgent,
  deleteAgent,
  fetchAvailableMcpServers,
  fetchAvailableSkills,
  fetchAgents,
  updateAgent,
} from "@/api/agents";

export { LearnedPromptSection } from "@/components/agentRoles/LearnedPromptSection";

// ── Role Card ────────────────────────────────────────────────────────────────

interface AgentCardProps {
  role: Agent;
  onEdit: () => void;
  onDelete: () => void;
  isDeleting: boolean;
  /** Editing agents (incl. their MCP servers + skills) is admin-only. */
  canEdit: boolean;
}

function AgentCard({ role, onEdit, onDelete, isDeleting, canEdit }: AgentCardProps) {
  const identity = getAgentIdentity(role.base_role);
  const mcpCount = role.mcp_servers?.length ?? 0;
  const skillCount = role.skills?.length ?? 0;
  const extCount = role.system_prompt_extensions.length;

  return (
    <div className="group relative flex flex-col rounded-xl border border-border bg-card overflow-hidden transition-colors hover:border-border/80">
      {/* Avatar area */}
      <div className="relative flex items-end justify-center bg-muted/30 pt-6 h-36">
        <img
          src={identity.avatar}
          alt={role.base_role}
          className="h-32 w-32 pointer-events-none"
        />
        {/* Hover actions — admin-only */}
        {canEdit && (
          <div className="absolute top-2 right-2 flex gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
            <Button variant="outline" size="sm" className="h-7 px-2 text-xs bg-card" onClick={onEdit}>
              Edit
            </Button>
            {!role.is_default && (
              <ConfirmButton
                title="Delete specialist"
                description={`Delete "${role.name}"? This cannot be undone.`}
                confirmLabel="Delete"
                onConfirm={onDelete}
                size="sm"
                disabled={isDeleting}
              >
                <HugeiconsIcon icon={Delete02Icon} size={14} />
              </ConfirmButton>
            )}
          </div>
        )}
      </div>

      {/* Info area */}
      <div className="flex flex-col flex-1 px-4 py-3 space-y-1.5">
        <div className="flex items-center gap-2">
          {!role.is_default && (
            <span className="font-medium text-sm truncate">{role.name}</span>
          )}
          <span className={cn("text-[11px] font-medium", identity.color)}>
            {identity.label}
          </span>
          {role.learned_prompt && (
            <span className="shrink-0 rounded-full w-2 h-2 bg-blue-500" title="Learned prompt active" />
          )}
          {role.is_default && (
            <span className="shrink-0 rounded-full bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">
              default
            </span>
          )}
        </div>
        {role.description && (
          <p className="text-xs text-muted-foreground line-clamp-2">{role.description}</p>
        )}

        {/* Compact counts */}
        {(extCount > 0 || mcpCount > 0 || skillCount > 0) && (
          <div className="flex flex-wrap gap-1.5 pt-1">
            {extCount > 0 && (
              <span className="rounded-full bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">
                {extCount} ext{extCount !== 1 ? "s" : ""}
              </span>
            )}
            {mcpCount > 0 && (
              <span className="rounded-full bg-blue-500/10 px-1.5 py-0.5 text-[10px] text-blue-700 dark:text-blue-300">
                {mcpCount} MCP
              </span>
            )}
            {skillCount > 0 && (
              <span className="rounded-full bg-purple-500/10 px-1.5 py-0.5 text-[10px] text-purple-700 dark:text-purple-300">
                {skillCount} skill{skillCount !== 1 ? "s" : ""}
              </span>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

// ── Main Component ───────────────────────────────────────────────────────────

export function AgentRoles() {
  const project = useSelectedProject();
  const isAdmin = useAuthUser()?.isAdmin ?? false;
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
        {isAdmin && (
          <Button onClick={() => setIsCreating(true)}>New Specialist</Button>
        )}
      </div>

      {error && <InlineError message={error} onRetry={() => void loadRoles()} />}

      {roles.length === 0 && !isCreating ? (
        <div className="rounded-lg border border-dashed border-border p-8 text-center text-sm text-muted-foreground">
          No roles configured yet. Create a specialist to extend a base role.
        </div>
      ) : (
        <div className="grid grid-cols-5 gap-3">
          {roles.map((role) => (
            <AgentCard
              key={role.id}
              role={role}
              onEdit={() => setEditingId(role.id)}
              onDelete={() => void handleDelete(role.id)}
              isDeleting={deletingId === role.id}
              canEdit={isAdmin}
            />
          ))}
        </div>
      )}
    </div>
  );
}
